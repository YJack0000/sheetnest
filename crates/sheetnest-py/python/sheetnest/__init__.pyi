"""Type stubs for the sheetnest extension module."""

from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple, Union

__version__: str

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

Point = Tuple[float, float]
RotationMode = str  # "orthogonal" | "free"
StopReason = str  # "time_limit" | "stale" | "cancelled" | "empty"

class Part:
    """One part to nest: an outer contour plus zero or more holes."""

    @staticmethod
    def from_polygon(
        name: str,
        quantity: int,
        outer: Sequence[Point],
        holes: Sequence[Sequence[Point]] = ...,
    ) -> "Part":
        """Build a part from plain vertex rings. Raises ValueError if degenerate."""

    @staticmethod
    def from_dxf(
        data: bytes,
        name: str,
        quantity: int = 1,
        curve_tolerance: float = 0.25,
    ) -> Tuple[List["Part"], List[str]]:
        """Parse DXF bytes into ``(parts, warnings)``. Raises ValueError if unreadable."""

    name: str
    quantity: int
    @property
    def gross_area(self) -> float: ...
    @property
    def net_area(self) -> float: ...
    @property
    def bbox(self) -> Tuple[float, float]: ...
    @property
    def hole_count(self) -> int: ...
    def __repr__(self) -> str: ...

class TabConfig:
    """Micro-joint settings."""

    def __init__(
        self,
        *,
        enabled: Optional[bool] = None,
        width: Optional[float] = None,
        max_spacing: Optional[float] = None,
        min_per_contour: Optional[int] = None,
        corner_clearance: Optional[float] = None,
        min_hole_size: Optional[float] = None,
    ) -> None: ...
    enabled: bool
    width: float
    max_spacing: float
    min_per_contour: int
    corner_clearance: float
    min_hole_size: float
    @staticmethod
    def from_dict(d: Mapping[str, Any]) -> "TabConfig": ...
    def to_dict(self) -> Dict[str, Any]: ...
    def __repr__(self) -> str: ...

class NestConfig:
    """Run settings. Every field is optional and falls back to the engine default."""

    def __init__(
        self,
        *,
        sheet_width: Optional[float] = None,
        sheet_height: Optional[float] = None,
        auto_width: Optional[bool] = None,
        spacing: Optional[float] = None,
        margin: Optional[float] = None,
        rotation_mode: Optional[RotationMode] = None,
        rotation_step_deg: Optional[float] = None,
        curve_tolerance: Optional[float] = None,
        time_limit_ms: Optional[int] = None,
        population: Optional[int] = None,
        mutation_rate: Optional[float] = None,
        stale_generations: Optional[int] = None,
        seed: Optional[int] = None,
        tabs: Union[TabConfig, Mapping[str, Any], None] = None,
    ) -> None: ...
    sheet_width: float
    sheet_height: float
    auto_width: bool
    spacing: float
    margin: float
    rotation_mode: RotationMode
    rotation_step_deg: float
    curve_tolerance: float
    time_limit_ms: int
    population: int
    mutation_rate: float
    stale_generations: int
    seed: Optional[int]
    tabs: TabConfig
    @staticmethod
    def from_dict(d: Mapping[str, Any]) -> "NestConfig":
        """Accepts camelCase or snake_case keys; missing keys use defaults."""

    def to_dict(self) -> Dict[str, Any]:
        """The config as a plain dict with snake_case keys."""

    def __repr__(self) -> str: ...

class Progress:
    """Per-generation snapshot handed to ``on_progress``."""

    @property
    def generation(self) -> int: ...
    @property
    def best_fitness(self) -> float: ...
    @property
    def best_utilization(self) -> float: ...
    @property
    def elapsed_ms(self) -> int: ...
    def __repr__(self) -> str: ...

class Placement:
    """One placed part instance: rotate by ``rotation_deg``, then translate."""

    @property
    def part_id(self) -> int: ...
    @property
    def part_name(self) -> str: ...
    @property
    def instance(self) -> int: ...
    @property
    def sheet(self) -> int: ...
    @property
    def rotation_deg(self) -> float: ...
    @property
    def dx(self) -> float: ...
    @property
    def dy(self) -> float: ...
    def __repr__(self) -> str: ...

class Stats:
    """Summary of a finished run."""

    @property
    def stop_reason(self) -> StopReason: ...
    @property
    def sheets_used(self) -> int: ...
    @property
    def used_width(self) -> float: ...
    @property
    def utilization(self) -> float: ...
    @property
    def strip_utilization(self) -> float: ...
    @property
    def generations(self) -> int: ...
    @property
    def elapsed_ms(self) -> int: ...
    @property
    def placed(self) -> int: ...
    @property
    def total(self) -> int: ...
    def __repr__(self) -> str: ...

class Solution:
    """The result of a run; renders itself to DXF or SVG."""

    @property
    def placements(self) -> List[Placement]: ...
    @property
    def stats(self) -> Stats: ...
    @property
    def sheet_width(self) -> float: ...
    @property
    def sheet_height(self) -> float: ...
    @property
    def warnings(self) -> List[str]: ...
    @property
    def config(self) -> NestConfig: ...
    @property
    def parts(self) -> List[Part]: ...
    def to_dict(self) -> Dict[str, Any]: ...
    def to_dxf(self) -> bytes: ...
    def to_svg(self, sheet: int = 0) -> str: ...
    def to_svg_all(self) -> str: ...
    def __repr__(self) -> str: ...

def nest(
    parts: Sequence[Part],
    config: Union[NestConfig, Mapping[str, Any], None] = None,
    *,
    on_progress: Optional[Callable[[Progress], None]] = None,
    should_stop: Optional[Callable[[], bool]] = None,
) -> Solution:
    """Nest ``parts`` onto sheets described by ``config``.

    The genetic algorithm runs with the GIL released; the hooks re-acquire it.
    Ctrl-C is polled once per generation and raises ``KeyboardInterrupt``.
    """
