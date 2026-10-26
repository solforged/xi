from decimal import Decimal
from .model import Entry

def parse(lines):
    for line in lines:
        invoice, currency, amount = line.rstrip('\n').split(',')
        yield Entry(invoice, currency, Decimal(amount))
