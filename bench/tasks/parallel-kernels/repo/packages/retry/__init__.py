def schedule(delays, *, base, cap, budget):
    starts=[0]
    for duration,retry_after in delays:
        starts.append(starts[-1]+duration+min(cap,base))
    return starts
