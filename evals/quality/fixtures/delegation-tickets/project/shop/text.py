"""Text helpers for product names and report labels."""


def slugify(name):
    """URL slug for a product name: lower case, every character but letters and digits turned into a dash."""
    out = []
    for char in name.lower():
        out.append(char if char.isalnum() else "-")
    return "".join(out)


def truncate(text, width):
    """Shorten `text` to at most `width` characters (width is at least 3)."""
    if len(text) <= width:
        return text
    return text[:width]
