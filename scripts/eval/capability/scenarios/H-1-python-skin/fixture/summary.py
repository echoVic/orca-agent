"""Slice statistics used by the report pipeline."""

from dataclasses import dataclass


@dataclass
class Summary:
    sum: int
    min: int
    max: int
    count: int


def summarize(values):
    """Compute the summary of ``values``.

    - ``sum`` is the total (values can be large).
    - ``min`` / ``max`` are the smallest / largest value.
    - ``count`` is the number of values.

    An empty list yields ``Summary(0, 0, 0, 0)``.
    """
    raise NotImplementedError("implement summarize")
