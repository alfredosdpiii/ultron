"""Per-column cell normalization."""
from decimal import ROUND_HALF_UP, Decimal, InvalidOperation

CENT = Decimal("0.01")


def normalize_email(cell):
    return cell.strip().lower()


def normalize_name(cell):
    return " ".join(cell.split())


def normalize_amount(cell):
    text = cell.strip()
    if not text:
        return ""
    negative = False
    if text.startswith("(") and text.endswith(")"):
        text = text[1:-1].strip()
    if text.startswith("-"):
        negative = True
        text = text[1:]
    if text.startswith("$"):
        text = text[1:]
    text = text.replace(",", "")
    try:
        value = Decimal(text)
    except InvalidOperation:
        raise ValueError(f"not an amount: {cell!r}") from None
    if not value.is_finite() or text.startswith(("-", "+")):
        raise ValueError(f"not an amount: {cell!r}")
    value = value.quantize(CENT, rounding=ROUND_HALF_UP)
    if negative and value != 0:
        value = -value
    return str(value)


NORMALIZERS = {"email": normalize_email, "name": normalize_name, "amount": normalize_amount}
