def chunks(items, size):
    """Yield lists of at most size items, preserving input order."""
    batch = []
    for item in items:
        batch.append(item)
        if len(batch) == size:
            yield batch
            batch = []
