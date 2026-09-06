"""Tests for the sheetnest Python bindings.

Run against an installed wheel (``maturin develop --release``) with pytest.
"""

from __future__ import annotations

import math
import signal
import threading
import time
from pathlib import Path

import pytest

import sheetnest
from sheetnest import NestConfig, Part, Progress, TabConfig, nest

FIXTURES = Path(__file__).resolve().parents[3] / "fixtures"
FIXTURE_NAMES = ["bracket_l", "disc", "gusset", "plate_rounded", "strip"]

SQUARE = [(0.0, 0.0), (100.0, 0.0), (100.0, 60.0), (0.0, 60.0)]
HOLE = [(20.0, 20.0), (30.0, 20.0), (30.0, 30.0), (20.0, 30.0)]


def fixture_parts(quantity: int = 1) -> list[Part]:
    parts: list[Part] = []
    for name in FIXTURE_NAMES:
        data = (FIXTURES / f"{name}.dxf").read_bytes()
        got, _warnings = Part.from_dxf(data, name, quantity)
        parts.extend(got)
    return parts


# --------------------------------------------------------------------------
# Part
# --------------------------------------------------------------------------


def test_from_polygon_round_trip() -> None:
    p = Part.from_polygon("plate", 3, SQUARE, [HOLE])
    assert p.name == "plate"
    assert p.quantity == 3
    assert p.hole_count == 1
    assert math.isclose(p.gross_area, 6000.0, rel_tol=1e-9)
    assert math.isclose(p.net_area, 5900.0, rel_tol=1e-9)
    w, h = p.bbox
    assert math.isclose(w, 100.0, rel_tol=1e-9)
    assert math.isclose(h, 60.0, rel_tol=1e-9)

    # Winding order and a repeated closing vertex are both normalized away.
    reversed_closed = list(reversed(SQUARE)) + [SQUARE[-1]]
    q = Part.from_polygon("plate", 3, reversed_closed, [])
    assert math.isclose(q.gross_area, 6000.0, rel_tol=1e-9)

    # Holes default to none.
    r = Part.from_polygon("plain", 1, SQUARE)
    assert r.hole_count == 0

    # name and quantity are settable.
    p.name = "renamed"
    p.quantity = 7
    assert p.name == "renamed"
    assert p.quantity == 7
    assert "renamed" in repr(p)


def test_from_polygon_rejects_two_point_ring() -> None:
    with pytest.raises(ValueError):
        Part.from_polygon("line", 1, [(0.0, 0.0), (5.0, 0.0)])


def test_from_polygon_rejects_degenerate_hole() -> None:
    with pytest.raises(ValueError):
        Part.from_polygon("bad", 1, SQUARE, [[(1.0, 1.0), (2.0, 2.0)]])


@pytest.mark.parametrize("name", FIXTURE_NAMES)
def test_from_dxf_fixture(name: str) -> None:
    data = (FIXTURES / f"{name}.dxf").read_bytes()
    parts, warnings = Part.from_dxf(data, name, 2)
    assert parts, f"{name}.dxf produced no parts"
    assert isinstance(warnings, list)
    for p in parts:
        assert p.quantity == 2
        assert p.gross_area > 0.0
        assert 0.0 < p.net_area <= p.gross_area
        w, h = p.bbox
        assert w > 0.0 and h > 0.0


def test_from_dxf_rejects_garbage() -> None:
    with pytest.raises(ValueError):
        Part.from_dxf(b"not a dxf at all", "junk")


def test_from_dxf_curve_tolerance_is_accepted() -> None:
    data = (FIXTURES / "disc.dxf").read_bytes()
    coarse, _ = Part.from_dxf(data, "disc", 1, 2.0)
    fine, _ = Part.from_dxf(data, "disc", 1, 0.01)
    assert coarse and fine
    # A finer chord tolerance can only grow the inscribed polygon's area.
    assert fine[0].gross_area >= coarse[0].gross_area - 1e-9


# --------------------------------------------------------------------------
# NestConfig
# --------------------------------------------------------------------------


def test_config_defaults_and_attributes() -> None:
    cfg = NestConfig()
    assert cfg.sheet_width == 1829.0
    assert cfg.sheet_height == 914.0
    assert cfg.rotation_mode == "orthogonal"
    assert cfg.seed is None
    assert cfg.tabs.enabled is False

    cfg.sheet_width = 2000.0
    cfg.rotation_mode = "free"
    cfg.seed = 42
    assert cfg.sheet_width == 2000.0
    assert cfg.rotation_mode == "free"
    assert cfg.seed == 42
    assert "NestConfig(" in repr(cfg)


def test_config_kwargs_and_tabs() -> None:
    cfg = NestConfig(
        sheet_width=1000.0,
        auto_width=True,
        seed=7,
        tabs={"enabled": True, "width": 0.5},
    )
    assert cfg.sheet_width == 1000.0
    assert cfg.auto_width is True
    assert cfg.seed == 7
    assert cfg.tabs.enabled is True
    assert cfg.tabs.width == 0.5
    # Untouched tab fields keep their defaults.
    assert cfg.tabs.min_per_contour == 2

    cfg2 = NestConfig(tabs=TabConfig(enabled=True, max_spacing=100.0))
    assert cfg2.tabs.enabled is True
    assert cfg2.tabs.max_spacing == 100.0


def test_config_rejects_bad_rotation_mode() -> None:
    with pytest.raises(ValueError):
        NestConfig(rotation_mode="diagonal")


def test_config_to_dict_from_dict_round_trip() -> None:
    cfg = NestConfig(sheet_width=1234.0, seed=5, rotation_mode="free", auto_width=True)
    d = cfg.to_dict()
    assert d["sheet_width"] == 1234.0
    assert d["rotation_mode"] == "free"
    assert d["seed"] == 5
    assert d["auto_width"] is True
    assert d["tabs"]["min_per_contour"] == 2
    # snake_case out, no camelCase leaking through.
    assert "sheetWidth" not in d

    back = NestConfig.from_dict(d)
    assert back.to_dict() == d


def test_config_from_dict_accepts_camel_case() -> None:
    camel = {
        "sheetWidth": 900.0,
        "sheetHeight": 400.0,
        "autoWidth": True,
        "rotationMode": "free",
        "timeLimitMs": 1234,
        "staleGenerations": 11,
        "seed": 3,
        "tabs": {"enabled": True, "maxSpacing": 120.0},
    }
    cfg = NestConfig.from_dict(camel)
    assert cfg.sheet_width == 900.0
    assert cfg.sheet_height == 400.0
    assert cfg.auto_width is True
    assert cfg.rotation_mode == "free"
    assert cfg.time_limit_ms == 1234
    assert cfg.stale_generations == 11
    assert cfg.seed == 3
    assert cfg.tabs.enabled is True
    assert cfg.tabs.max_spacing == 120.0
    # Unspecified fields fall back to the engine defaults.
    assert cfg.population == 15

    d = cfg.to_dict()
    assert NestConfig.from_dict(d).to_dict() == d


def test_tab_config_dict_round_trip() -> None:
    t = TabConfig(enabled=True, width=0.4)
    d = t.to_dict()
    assert d["enabled"] is True
    assert d["width"] == 0.4
    assert d["max_spacing"] == 250.0
    assert TabConfig.from_dict(d).to_dict() == d
    assert TabConfig.from_dict({"maxSpacing": 300.0}).max_spacing == 300.0


# --------------------------------------------------------------------------
# nest()
# --------------------------------------------------------------------------

RUN = dict(seed=1, stale_generations=30, time_limit_ms=20000)


@pytest.fixture(scope="module")
def solution():
    return nest(fixture_parts(), NestConfig(**RUN))


def test_nest_places_every_fixture(solution) -> None:
    stats = solution.stats
    assert stats.total == len(FIXTURE_NAMES)
    assert stats.placed == stats.total
    assert stats.sheets_used >= 1
    assert stats.used_width > 0.0
    assert 0.0 < stats.utilization <= 1.0
    assert stats.generations >= 1
    assert stats.stop_reason in {"stale", "time_limit"}
    assert solution.sheet_width > 0.0
    assert solution.sheet_height > 0.0
    assert isinstance(solution.warnings, list)
    assert "Solution(" in repr(solution)

    assert len(solution.placements) == stats.placed
    names = {p.name for p in solution.parts}
    for pl in solution.placements:
        assert 0 <= pl.part_id < len(FIXTURE_NAMES)
        assert pl.part_name in names
        assert pl.sheet < stats.sheets_used
        assert pl.rotation_deg in (0.0, 90.0, 180.0, 270.0)
        assert "Placement(" in repr(pl)


def test_nest_is_deterministic_with_a_seed() -> None:
    cfg = NestConfig(**RUN)
    a = nest(fixture_parts(), cfg)
    b = nest(fixture_parts(), cfg)
    # The determinism guarantee holds when the run ends on stale_generations,
    # not on the wall clock.
    assert a.stats.stop_reason == "stale"
    assert b.stats.stop_reason == "stale"
    assert a.to_dict()["placements"] == b.to_dict()["placements"]
    assert a.stats.used_width == b.stats.used_width


def test_nest_accepts_a_plain_dict_config() -> None:
    sol = nest(fixture_parts(), {"sheetWidth": 2000.0, **RUN})
    assert sol.config.sheet_width == 2000.0
    assert sol.stats.placed == sol.stats.total


def test_nest_with_no_config_uses_defaults() -> None:
    # One tiny part, so the default 20s limit is never the binding constraint.
    sol = nest([Part.from_polygon("plate", 1, SQUARE)], NestConfig(stale_generations=5, seed=1))
    assert sol.stats.placed == 1


def test_to_dict_is_snake_case(solution) -> None:
    d = solution.to_dict()
    assert set(d) >= {"placements", "stats", "sheet_width", "sheet_height", "warnings"}
    assert "sheetWidth" not in d
    assert set(d["placements"][0]) == {
        "part_id",
        "part_name",
        "instance",
        "sheet",
        "rotation_deg",
        "dx",
        "dy",
    }
    assert d["stats"]["stop_reason"] == solution.stats.stop_reason
    assert "stopReason" not in d["stats"]


def test_on_progress_receives_progress() -> None:
    seen: list[Progress] = []
    sol = nest(fixture_parts(), NestConfig(**RUN), on_progress=seen.append)
    assert len(seen) >= 1
    first = seen[0]
    assert isinstance(first, Progress)
    assert first.generation >= 0
    assert isinstance(first.best_fitness, float)
    assert isinstance(first.best_utilization, float)
    assert first.elapsed_ms >= 0
    assert "Progress(" in repr(first)
    assert seen[-1].generation >= first.generation
    assert sol.stats.generations >= 1


def test_on_progress_exception_propagates() -> None:
    class Boom(Exception):
        pass

    def blow_up(_progress: Progress) -> None:
        raise Boom("nope")

    with pytest.raises(Boom):
        nest(fixture_parts(), NestConfig(**RUN), on_progress=blow_up)


def test_should_stop_cancels() -> None:
    sol = nest(fixture_parts(), NestConfig(**RUN), should_stop=lambda: True)
    assert sol.stats.stop_reason == "cancelled"


def test_should_stop_exception_propagates() -> None:
    def blow_up() -> bool:
        raise RuntimeError("stop hook failed")

    with pytest.raises(RuntimeError, match="stop hook failed"):
        nest(fixture_parts(), NestConfig(**RUN), should_stop=blow_up)


def test_nest_releases_the_gil() -> None:
    ticks = 0
    done = threading.Event()

    def spin() -> None:
        nonlocal ticks
        while not done.is_set():
            ticks += 1
            time.sleep(0.001)

    worker = threading.Thread(target=spin)
    worker.start()
    try:
        # Never goes stale, so it runs for the full (short) time limit.
        sol = nest(fixture_parts(4), NestConfig(seed=1, stale_generations=10**9, time_limit_ms=1500))
    finally:
        done.set()
        worker.join()
    assert sol.stats.stop_reason == "time_limit"
    # With the GIL held for the whole run this would be ~0.
    assert ticks > 50, f"background thread only ran {ticks} times; GIL was not released"


def test_ctrl_c_interrupts_a_run() -> None:
    def interrupt() -> None:
        time.sleep(0.5)
        signal.raise_signal(signal.SIGINT)

    threading.Thread(target=interrupt, daemon=True).start()
    started = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        # Would otherwise run for 30s.
        nest(fixture_parts(8), NestConfig(seed=1, stale_generations=10**9, time_limit_ms=30000))
    assert time.monotonic() - started < 20.0


def test_nest_of_nothing() -> None:
    sol = nest([], NestConfig(**RUN))
    assert sol.stats.placed == 0
    assert sol.stats.total == 0
    assert sol.stats.stop_reason == "empty"


# --------------------------------------------------------------------------
# output
# --------------------------------------------------------------------------


def test_to_dxf_is_a_dxf(solution) -> None:
    data = solution.to_dxf()
    assert isinstance(data, bytes)
    assert b"SECTION" in data[:4096]
    assert b"SHEET" in data
    assert b"EOF" in data[-64:]


def test_to_svg(solution) -> None:
    svg = solution.to_svg()
    assert "<svg" in svg
    assert "</svg>" in svg
    assert solution.to_svg(0) == svg
    all_svg = solution.to_svg_all()
    assert "<svg" in all_svg
    assert len(all_svg) >= len(svg)


def test_module_surface() -> None:
    assert isinstance(sheetnest.__version__, str)
    for name in sheetnest.__all__:
        assert hasattr(sheetnest, name)
