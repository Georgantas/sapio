// Copyright Judica, Inc 2021
//
// This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The primary compilation traits and types
use super::actions::ConditionalCompileType;
use super::AnyContract;
use super::CompilationError;
use super::Compiled;
use super::Context;
use crate::contract::abi::continuation::ContinuationPoint;
use crate::contract::actions::conditional_compile::CCILWrapper;
use crate::contract::actions::CallableAsFoF;
use crate::contract::TxTmplIt;
use crate::util::amountrange::AmountRange;
use ::miniscript::descriptor::TapTree;
use ::miniscript::*;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::schnorr::TweakedPublicKey;
use std::collections::BinaryHeap;

use bitcoin::XOnlyPublicKey;
use sapio_base::effects::EffectDB;
use sapio_base::effects::EffectPath;
use sapio_base::effects::PathFragment;
use sapio_base::serialization_helpers::SArc;
use sapio_base::Clause;
use std::cmp::Reverse;

use std::collections::HashMap;
use std::collections::LinkedList;
use std::sync::Arc;
mod cache;
use cache::*;
/// Used to prevent unintended callers to internal_clone.
pub struct InternalCompilerTag {
    _secret: (),
}

/// private::ImplSeal prevents anyone from implementing Compilable except by
/// implementing Contract.
mod private {
    pub trait ImplSeal {}
    /// Allow Contract to implement Compile
    impl ImplSeal for super::Compiled {}
    impl ImplSeal for bitcoin::XOnlyPublicKey {}
    impl<'a, C> ImplSeal for C where C: super::AnyContract {}
}
/// Compilable is a trait for anything which can be compiled
pub trait Compilable: private::ImplSeal {
    /// Compile a compilable object returning errors, if any.
    fn compile(&self, ctx: Context) -> Result<Compiled, CompilationError>;
}

/// Implements a basic identity
impl Compilable for Compiled {
    fn compile(&self, _ctx: Context) -> Result<Compiled, CompilationError> {
        Ok(self.clone())
    }
}

impl Compilable for bitcoin::XOnlyPublicKey {
    // TODO: Taproot; make infallible API
    fn compile(&self, ctx: Context) -> Result<Compiled, CompilationError> {
        let addr = bitcoin::Address::p2tr_tweaked(
            TweakedPublicKey::dangerous_assume_tweaked(self.clone()),
            ctx.network,
        );
        let mut amt = AmountRange::new();
        amt.update_range(ctx.funds());
        Ok(Compiled::from_address(addr, Some(amt)))
    }
}

#[derive(PartialEq, Eq)]
enum UseCTV {
    Yes,
    No,
}
#[derive(PartialEq, Eq)]
enum Nullable {
    Yes,
    No,
}

fn compute_all_effects<C, A: Default>(
    mut top_effect_ctx: Context,
    self_ref: &C,
    func: &dyn CallableAsFoF<C, A>,
) -> TxTmplIt {
    let mut applied_effects_ctx = top_effect_ctx.derive(PathFragment::Effects)?;
    let default_applied_effect_ctx = top_effect_ctx.derive(PathFragment::DefaultEffect)?;
    top_effect_ctx
        .get_effects(InternalCompilerTag { _secret: () })
        .get_value(top_effect_ctx.path())
        .flat_map(|(k, arg)| {
            let c = applied_effects_ctx
                .derive(PathFragment::Named(SArc(k.clone())))
                .expect("Must be a valid derivation or internal invariant not held");
            func.call_json(self_ref, c, arg.clone())
        })
        // always gets the default expansion, but will also attempt
        // operating with the effects passed in through the Context Object.:write!
        .fold(
            func.call(self_ref, default_applied_effect_ctx, Default::default()),
            |a: TxTmplIt, b: TxTmplIt| -> TxTmplIt {
                match (a, b) {
                    (Err(x), _) => Err(x),
                    (_, Err(y)) => Err(y),
                    (Ok(v), Ok(w)) => Ok(Box::new(v.chain(w))),
                }
            },
        )
}

impl<'a, T> Compilable for T
where
    T: AnyContract + 'a,
    T::Ref: 'a,
{
    /// The main Compilation Logic for a Contract.
    /// TODO: Better Document Semantics
    fn compile(&self, mut ctx: Context) -> Result<Compiled, CompilationError> {
        let self_ref = self.get_inner_ref();

        let guard_clauses = std::cell::RefCell::new(GuardCache::new());

        // The code for then_fns and finish_or_fns is very similar, differing
        // only in that then_fns have a CTV enforcing the contract and
        // finish_or_fns do not. We can lazily chain iterators to process them
        // in a row.
        let then_fns: Vec<_> = {
            let mut then_fn_ctx = ctx.derive(PathFragment::ThenFn)?;
            let mut conditional_compile_ctx = then_fn_ctx.derive(PathFragment::CondCompIf)?;
            let mut guards_ctx = then_fn_ctx.derive(PathFragment::Guard)?;
            let mut next_tx_ctx = then_fn_ctx.derive(PathFragment::Next)?;
            self.then_fns()
                .iter()
                .filter_map(|func| func())
                .flat_map(|func| {
                    let name = PathFragment::Named(SArc(func.name.clone()));
                    conditional_compile_ctx
                        .derive(name.clone())
                        .map(|mut this_ctx| {
                            match CCILWrapper(func.conditional_compile_if)
                                .assemble(self_ref, &mut this_ctx)
                            {
                                ConditionalCompileType::Fail(errors) => {
                                    Some((func, name, (errors, Nullable::No)))
                                }
                                ConditionalCompileType::Required
                                | ConditionalCompileType::NoConstraint => {
                                    Some((func, name, (LinkedList::new(), Nullable::No)))
                                }
                                ConditionalCompileType::Nullable => {
                                    Some((func, name, (LinkedList::new(), Nullable::Yes)))
                                }
                                ConditionalCompileType::Skippable
                                | ConditionalCompileType::Never => None,
                            }
                        })
                        .transpose()
                })
                .map(|r| {
                    r.and_then(|(func, name, (errors, nullability))| {
                        let gctx = guards_ctx.derive(name.clone())?;
                        let ntx_ctx = next_tx_ctx.derive(name)?;
                        let guards = create_guards(
                            self_ref,
                            gctx,
                            func.guard,
                            &mut guard_clauses.borrow_mut(),
                        );
                        Ok((
                            nullability,
                            UseCTV::Yes,
                            guards,
                            if errors.is_empty() {
                                (func.func)(self_ref, ntx_ctx)
                            } else {
                                Err(CompilationError::ConditionalCompilationFailed(errors))
                            },
                        ))
                    })
                })
                .collect::<Result<Vec<_>, CompilationError>>()?
        };
        // finish_or_fns may be used to compute additional transactions with
        // a given argument, but for building the ABI we only precompute with
        // the default argument.
        let (continue_apis, finish_or_fns): (
            HashMap<SArc<EffectPath>, ContinuationPoint>,
            Vec<(Nullable, UseCTV, Clause, TxTmplIt)>,
        ) = {
            let mut finish_or_fns_ctx = ctx.derive(PathFragment::FinishOrFn)?;
            let mut conditional_compile_ctx = finish_or_fns_ctx.derive(PathFragment::CondCompIf)?;
            let mut guard_ctx = finish_or_fns_ctx.derive(PathFragment::Guard)?;
            let mut suggested_tx_ctx = finish_or_fns_ctx.derive(PathFragment::Suggested)?;
            self.finish_or_fns()
                .iter()
                .filter_map(|func| func())
                // TODO: De-duplicate this code?
                .filter_map(|func| {
                    let name = PathFragment::Named(SArc(func.get_name().clone()));
                    conditional_compile_ctx
                        .derive(name.clone())
                        .map(|mut this_ctx| {
                            let constraint = CCILWrapper(func.get_conditional_compile_if())
                                .assemble(self_ref, &mut this_ctx);
                            match constraint {
                                ConditionalCompileType::Fail(errors) => Some((func, name, errors)),
                                ConditionalCompileType::Required
                                | ConditionalCompileType::NoConstraint
                                | ConditionalCompileType::Nullable => {
                                    Some((func, name, LinkedList::new()))
                                }
                                ConditionalCompileType::Skippable
                                | ConditionalCompileType::Never => None,
                            }
                        })
                        .transpose()
                })
                .map(|r| {
                    r.and_then(|(func, name, errors)| {
                        let top_effect_ctx = suggested_tx_ctx.derive(name.clone())?;
                        let guard = create_guards(
                            self_ref,
                            guard_ctx.derive(name)?,
                            func.get_guard(),
                            &mut guard_clauses.borrow_mut(),
                        );
                        Ok((
                            (
                                SArc(top_effect_ctx.path().clone()),
                                ContinuationPoint::at(
                                    func.get_schema().clone(),
                                    top_effect_ctx.path().clone(),
                                ),
                            ),
                            (
                                Nullable::Yes,
                                UseCTV::No,
                                guard,
                                if errors.is_empty() {
                                    compute_all_effects(top_effect_ctx, self_ref, func.as_ref())
                                } else {
                                    Err(CompilationError::ConditionalCompilationFailed(errors))
                                },
                            ),
                        ))
                    })
                })
                .collect::<Result<
                    Vec<(
                        (SArc<EffectPath>, ContinuationPoint),
                        (Nullable, UseCTV, Clause, TxTmplIt),
                    )>,
                    CompilationError,
                >>()?
                .into_iter()
                .unzip()
        };

        let mut ctv_to_tx = HashMap::new();
        let mut suggested_txs = HashMap::new();
        let mut amount_range = AmountRange::new();

        // If no guards and not CTV, then nothing gets added (not interpreted as Trivial True)
        // If CTV and no guards, just CTV added.
        // If CTV and guards, CTV & guards added.
        let clause_accumulator = then_fns
            .into_iter()
            .chain(finish_or_fns.into_iter())
            .map(|(nullability, uses_ctv, guards, r_txtmpls)| {
                // it would be an error if any of r_txtmpls is an error instead of just an empty
                // iterator.
                let txtmpl_clauses = r_txtmpls?
                    .map(|r_txtmpl| {
                        let txtmpl = r_txtmpl?;
                        let h = txtmpl.hash();
                        amount_range.update_range(txtmpl.max);
                        // Add the addition guards to these clauses
                        if uses_ctv == UseCTV::Yes {
                            let txtmpl = ctv_to_tx.entry(h).or_insert(txtmpl);
                            if txtmpl.guards.len() == 0 {
                                ctx.ctv_emulator(h).map(Some)
                            } else {
                                let mut g = txtmpl.guards.clone();
                                g.push(ctx.ctv_emulator(h)?);
                                Ok(Some(Clause::And(g)))
                            }
                        } else {
                            let txtmpl = suggested_txs.entry(h).or_insert(txtmpl);
                            // Don't return or use the extra guards here because we're within a
                            // non-CTV context... if we did, then it would destabilize compilation
                            // with effect arguments.
                            if txtmpl.guards.len() != 0 {
                                // todo: In theory, the *default* effect could pass up something here.
                                Err(CompilationError::AdditionalGuardsNotAllowedHere)
                            } else {
                                // Don't add anything...
                                Ok(None)
                            }
                        }
                    })
                    // Drop None values
                    .filter_map(|s| s.transpose())
                    // Forces any error to abort the whole thing
                    .collect::<Result<Vec<Clause>, CompilationError>>()?;

                match (uses_ctv, nullability, txtmpl_clauses.len(), guards) {
                    // Mark this branch dead.
                    // Nullable branch without anything
                    (UseCTV::Yes, Nullable::Yes, 0, _) => Ok(vec![]),
                    // Error if we expect CTV, returned some templates, but our guard
                    // was unsatisfiable, irrespective of nullability. This is because
                    // the behavior should be captured through a compile_if if it is
                    // intended.
                    (UseCTV::Yes, _, n, Clause::Unsatisfiable) if n > 0 => {
                        // TODO: Turn into a warning that the intended behavior should be to compile_if
                        Err(CompilationError::MissingTemplates)
                    }
                    // Error if 0 templates return and we don't want to be nullable
                    (UseCTV::Yes, Nullable::No, 0, _) => Err(CompilationError::MissingTemplates),
                    // If the guard is trivial, return the hashes standalone
                    (UseCTV::Yes, _, _, Clause::Trivial) => Ok(txtmpl_clauses),
                    // If the guard is non-trivial, zip it to each hash
                    // TODO: Arc in miniscript to dedup memory?
                    //       This could be Clause::Shared(x) or something...
                    (_, _, _, guards) => Ok(txtmpl_clauses
                        .into_iter()
                        // extra_guards will contain any CTV
                        .map(|extra_guards| Clause::And(vec![guards.clone(), extra_guards]))
                        .collect()),
                }
            })
            .collect::<Result<Vec<Vec<Clause>>, CompilationError>>()?;
        let finish_fns: Vec<_> = {
            let mut finish_fns_ctx = ctx.derive(PathFragment::FinishFn)?;
            // Compute all finish_functions at this level, caching if requested.
            self.finish_fns()
                .iter()
                // note that this zip with would loop forever if there were to be a bug here
                .zip(
                    (0..)
                        .filter_map(|i| finish_fns_ctx.derive(PathFragment::Branch(i as u64)).ok()),
                )
                .filter_map(|(func, c)| guard_clauses.borrow_mut().get(self_ref, *func, c))
                .collect()
        };
        let branches: Vec<Miniscript<XOnlyPublicKey, Tap>> = finish_fns
            .iter()
            .chain(clause_accumulator.iter().flatten())
            .map(|policy| policy.compile().map_err(Into::<CompilationError>::into))
            .collect::<Result<Vec<_>, _>>()?;
        // TODO: Pick a better branch that is guaranteed to work!
        let some_key = branches
            .iter()
            .filter_map(|f| {
                if let Terminal::PkK(k) = f.node {
                    Some(k)
                } else {
                    None
                }
            })
            .next()
            .map(|x| bitcoin::util::schnorr::UntweakedPublicKey::from(x))
            .unwrap_or(
                XOnlyPublicKey::from_slice(&Sha256::hash(&[1u8; 32]).into_inner())
                    .expect("constant"),
            );
        // Don't remove the key from the scripts in case it was bogus
        let mut scripts: BinaryHeap<(Reverse<u64>, TapTree<XOnlyPublicKey>)> = branches
            .iter()
            .map(|b| (Reverse(1), TapTree::Leaf(Arc::new(b.clone()))))
            .collect();
        while scripts.len() > 1 {
            let (w1, v1) = scripts.pop().unwrap();
            let (w2, v2) = scripts.pop().unwrap();
            scripts.push((
                Reverse(w1.0.saturating_add(w2.0)),
                TapTree::Tree(Arc::new(v1), Arc::new(v2)),
            ));
        }

        let tree = scripts.pop().map(|v| v.1);
        let descriptor = Descriptor::Tr(descriptor::Tr::new(some_key, tree)?);
        let estimated_max_size = descriptor.max_satisfaction_weight()?;
        let address = descriptor.address(ctx.network)?.into();
        let descriptor = Some(descriptor.into());
        let root_path = SArc(ctx.path().clone());

        let failed_estimate = ctv_to_tx.values().any(|a| {
            // witness space not scaled
            let tx_size = a.tx.get_weight() + estimated_max_size;
            let fees = amount_range.max() - a.total_amount();
            a.min_feerate_sats_vbyte
                .map(|m| fees.as_sat() < (m.as_sat() * tx_size as u64))
                == Some(false)
        });
        if failed_estimate {
            Err(CompilationError::MinFeerateError)
        } else {
            Ok(Compiled {
                ctv_to_tx,
                suggested_txs,
                continue_apis,
                root_path,
                address,
                descriptor,
                amount_range,
            })
        }
    }
}
