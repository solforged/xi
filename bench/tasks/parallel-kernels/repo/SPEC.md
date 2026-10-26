# Independent package repairs

## csvstream

Stream incremental CSV rows. feed(chunk, final=False) returns newly completed rows. Delimiters are commas; quotes can begin only at the start of a field; doubled quotes represent a literal quote. LF or CRLF terminates a row outside quotes; quoted newlines are preserved. A bare CR outside quotes is invalid, even across chunks. A trailing comma creates an empty last field. An empty stream yields no rows; a newline yields one empty field. At final=True, emit a pending unterminated row but reject unclosed quotes, bare CR, or text after a closing quote. Reject every feed after final. Chunk boundaries must not matter.

## overlay

paint(iterable) takes (start,end,label) half-open integer intervals in application order, last paint wins. Ignore zero-length intervals, reject reversed or negative bounds with ValueError. Return sorted, nonoverlapping, adjacent-equal-label-coalesced triples. Do not iterate every coordinate: endpoints can exceed 10**18. Do not mutate input. Accept one-shot iterators and labels including None and empty strings.

## waves

plan(mapping) accepts job -> iterable of prerequisite jobs. Return alphabetically sorted simultaneous waves: a job runs in the first wave after all its prerequisites have completed. Validate unknown dependencies and cycles (including self loops) with ValueError. Duplicate edges are harmless. Values may be generators. Do not mutate the mapping. Jobs without dependencies all share the first wave; an empty mapping returns [].

## pointer

resolve(document, pointer) implements RFC 6901 string-form JSON pointers. Empty pointer returns the original document by identity; otherwise require leading /. Decode ~1 to / and ~0 to ~ once, in that order, rejecting other or incomplete tilde escapes. Dictionary keys are exact strings including empty, numeric and -. Array tokens are canonical ASCII decimal indexes (0 or nonzero without leading zeroes), not -, signs or Unicode digits; reject out-of-range indexes. Fail all invalid pointers, missing keys and scalar traversal with ValueError. Never mutate the document.

## retry

schedule(delays, *, base, cap, budget) returns integer start times for attempts. Attempt zero starts at 0 and is always included. Each item is (duration, retry_after): after that attempt fails, compute wait=max(min(cap, base*2**index), retry_after or 0), where index starts at zero for the first failure and retry_after is a server minimum not capped. The next start is prior start+duration+wait and is included iff <=budget; stop permanently at the first over-budget next attempt. Reject invalid parameters and every malformed/negative item, including items beyond the budget cutoff. Values must be integers, not booleans; base/cap must be positive and budget nonnegative. retry_after may be None or nonnegative integer. No overflow or huge exponent allocation for very long input; cap the exponent growth. Accept generators.
