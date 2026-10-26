from urllib.parse import urlsplit, urlunsplit


def cache_key(method, url):
    """Build a method/URL key; fragments never reach the server."""
    parts = urlsplit(url)
    return method.upper(), urlunsplit((parts.scheme, parts.netloc, parts.path, parts.query, ""))
