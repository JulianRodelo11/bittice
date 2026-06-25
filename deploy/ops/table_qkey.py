"""Resolve flexible table filters against bootstrapped CDC qkeys (schema.table).

Used by repair/diagnose shell scripts. Logic mirrors `mirror_consistency::matches_table_filter`.
"""
from __future__ import annotations


def qkey_table(qkey: str) -> str:
    return qkey.rsplit(".", 1)[-1]


def qkey_schema(qkey: str) -> str | None:
    if "." not in qkey:
        return None
    return qkey.rsplit(".", 1)[0]


def matches_table_filter(qkey: str, filt: str) -> bool:
    filt = (filt or "").strip()
    if not filt:
        return True
    if qkey.lower() == filt.lower():
        return True

    filt = filt.replace("/", ".")

    if "." in filt:
        filter_schema, filter_table = filt.rsplit(".", 1)
        if not filter_schema:
            return qkey_table(qkey).lower() == filter_table.lower()
        schema = qkey_schema(qkey)
        if schema is None:
            return False
        if qkey_table(qkey).lower() != filter_table.lower():
            return False
        return (
            schema.lower() == filter_schema.lower()
            or filter_schema.lower() in schema.lower()
        )

    return qkey_table(qkey).lower() == filt.lower()


def _prefer_prod(matches: list[str]) -> list[str]:
    if len(matches) <= 1:
        return matches
    prod = [q for q in matches if "_prod" in q.lower()]
    return prod if prod else matches


def resolve_table_filters(bootstrapped: list[str], filters: list[str]) -> list[str]:
    """Expand user patterns to concrete bootstrapped qkeys (deduped, stable order)."""
    out: list[str] = []
    seen: set[str] = set()
    for filt in filters:
        matched = sorted(q for q in bootstrapped if matches_table_filter(q, filt))
        matched = _prefer_prod(matched)
        if not matched:
            raise ValueError(f"no bootstrapped table matches filter {filt!r}")
        for q in matched:
            if q not in seen:
                seen.add(q)
                out.append(q)
    return out


DEFAULT_ATTENDANT_DRIFT_PATTERNS = (
    "attendant.pagos",
    "attendant.entradaVehiculos",
    "attendant.transacciones",
)
