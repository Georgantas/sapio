from typing import TypedDict, List, Tuple
from jsonschema import Draft7Validator
from sapio_server.context import Context
from sapio_server.network.json import address
from bitcoinlib.static_types import Amount
from sapio_compiler import BindableContract

schema = {
    "$schema": "http://json-schema.org/draft-07/schema#",
    "type": "array",
    "items": address.schema,
}
validator = Draft7Validator(schema)


PayDict = List[address.AddrDict]


def convert(arg: PayDict, ctx: Context) -> List[Tuple[Amount, BindableContract]]:
    validator.validate(arg)
    return list(map(lambda p: address.convert(p, ctx), arg))
