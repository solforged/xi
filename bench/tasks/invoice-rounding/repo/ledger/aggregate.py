from decimal import Decimal

def totals(entries, options):
    result = {}
    quantum = Decimal(10) ** -options.precision
    for entry in entries:
        result[entry.currency] = result.get(entry.currency, Decimal(0)) + entry.amount.quantize(quantum)
    return result
