from .model import Options

def load(mapping):
    return Options(precision=mapping.get('precision', 2))

def override(options, *, precision=None, rounding=None, scope=None):
    return Options(precision=precision or options.precision)
