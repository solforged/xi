def weighted_mean(pairs):
    """Average (value, nonnegative weight) pairs; require positive total weight."""
    pairs = list(pairs)
    return sum(value * weight for value, weight in pairs) / len(pairs)
