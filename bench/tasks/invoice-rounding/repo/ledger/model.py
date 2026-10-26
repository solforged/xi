from dataclasses import dataclass
from decimal import Decimal

@dataclass(frozen=True)
class Entry:
    invoice: str
    currency: str
    amount: Decimal

@dataclass(frozen=True)
class Options:
    precision: int = 2
