from sapio.contract import Contract, TransactionTemplate, unlock, path
from sapio.script.clause import *


class UndoSend(Contract):
    class Fields:
        from_contract: Contract
        to_key: PubKey
        amount: Amount
        timeout: TimeSpec

    @unlock(lambda self: AfterClause(self.timeout)*SignatureCheckClause(self.to_key))
    def _(self): pass

    @path(lambda self: SignatureCheckClause(self.to_key))
    def undo(self) -> TransactionTemplate:
        tx = TransactionTemplate()
        tx.add_output(self.amount.assigned_value, self.from_contract.assigned_value)
        return tx

class UndoSend2(Contract):
    class Fields:
        from_contract: Contract
        to_contract: Contract
        amount: Amount
        timeout: TimeSpec

    class MetaData:
        color = lambda self: "red"
        label = lambda self: "Undo Send"

    @path
    def complete(self) -> TransactionTemplate:
        tx = TransactionTemplate()
        tx.set_sequence(self.timeout.assigned_value.time)
        tx.add_output(self.amount.assigned_value, self.to_contract.assigned_value)
        return tx

    @path
    def undo(self) -> TransactionTemplate:
        tx = TransactionTemplate()
        tx.add_output(self.amount.assigned_value, self.from_contract.assigned_value)
        return tx
