// Copyright Judica, Inc 2021
//
// This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[deny(missing_docs)]
use batching_trait::BatchingTraitVersion0_1_1;

use sapio::contract::*;
use sapio::*;
use sapio_wasm_plugin::client::*;
use sapio_wasm_plugin::*;
use schemars::*;
use serde::*;
use std::sync::Arc;

/// # Trampolined Payment
/// A payment which is passed off to another program via a trait-locked plugin
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct TrampolinePay {
    /// # Which Plugin to Use
    /// Specify which contract plugin to call out to.
    handle: SapioHostAPI<BatchingTraitVersion0_1_1>,
    /// # Data for the Contract
    // Just do this to get the data... not always necessary (could be computed any way)
    data: BatchingTraitVersion0_1_1,
}

/// # Versions Trait Wrapper
#[derive(Serialize, Deserialize, JsonSchema)]
enum Versions {
    /// # Batching Trait API
    BatchingTraitVersion0_1_1(BatchingTraitVersion0_1_1),
}
impl TrampolinePay {
    #[then]
    fn expand(self, mut ctx: Context) {
        let contract = create_contract_by_key(
            ctx.derive_str(Arc::new("plugin_trampoline".into()))?,
            &self.handle.key,
            CreateArgs {
                context: ContextualArguments {
                    amount: ctx.funds(),
                    network: ctx.network,
                    effects: unsafe { ctx.get_effects_internal() }.as_ref().clone(),
                },
                arguments: Versions::BatchingTraitVersion0_1_1(self.data.clone()),
            },
        )?;
        let mut builder = ctx.template();
        builder = builder.add_output(contract.amount_range.max(), &contract, None)?;
        builder.into()
    }
}
impl Contract for TrampolinePay {
    declare! {then, Self::expand}
    declare! {non updatable}
}

REGISTER![TrampolinePay, "logo.png"];
