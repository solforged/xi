import json

def render(totals, options):
    return json.dumps({key: float(value) for key, value in totals.items()})
