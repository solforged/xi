# Invoice rounding

Add two options to the existing precision option: rounding (`even` or `away`, default `even`) and scope (`line` or `invoice`, default `line`). Precision is an integer from 0 through 6, not a boolean. Reject unknown config keys and invalid values with ValueError. CLI flags override config values only when supplied, including zero precision. Do not mutate supplied mappings or options.

CSV input has exactly three nonempty fields: invoice, uppercase three-letter ASCII currency, and a finite Decimal amount. Use the CSV dialect's quoting rules so invoice ids can contain commas. Trim whitespace around fields. Blank lines are skipped. Reject malformed rows, nonfinite amounts, and invalid currency codes with ValueError mentioning the one-based physical line number. Iterators must work.

At line scope, round each entry before adding it to its currency total. At invoice scope, first sum each (invoice, currency) group, round each group, then add to currency totals. The same invoice id in two currencies is not one group. `even` uses ties to even; `away` rounds ties away from zero (not every non-tie). Decimal context precision and rounding set by a caller must not affect the answer or be modified. Support amounts with up to 50 significant digits and 20 fractional digits, even when intermediate sums need more precision. Output sorted currency keys, exact fixed-width decimal strings with the requested precision, and never negative zero. Empty input is {}. JSON output contains no floats or exponent notation.

CLI: python3 -m ledger.cli INPUT [--config JSON] [--precision N] [--rounding even|away] [--scope line|invoice]. Keep existing defaults. Tests exercise each layer and CLI end to end.
