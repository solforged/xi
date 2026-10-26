def render_label(name, count):
    """Render a stripped name and count without changing name casing."""
    return f"{name.strip() or '(unnamed)'}: {count}"
