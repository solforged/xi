from labels import render_label


def report(rows):
    return "\n".join(render_label(name, count) for name, count in rows)


def total_line(count):
    return render_label("Total", count)
