"""2D nesting for sheet cutting.

Pack parts onto rectangular stock with as little waste as possible, using a
no-fit-polygon placer driven by a genetic algorithm. Arcs stay analytic all
the way to the DXF output, so a laser or plasma cutter gets true ``ARC``
entities.

All lengths are millimeters, all angles degrees, y is up.
"""

from ._sheetnest import (
    NestConfig,
    Part,
    Placement,
    Progress,
    Solution,
    Stats,
    TabConfig,
    __version__,
    nest,
)

__all__ = [
    "NestConfig",
    "Part",
    "Placement",
    "Progress",
    "Solution",
    "Stats",
    "TabConfig",
    "__version__",
    "nest",
]
