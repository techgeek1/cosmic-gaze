"""The JSON line protocol: field set, types, nullability, round trip."""

from __future__ import annotations

import json
import math

import numpy as np
import pytest

from gaze_ml import schema


def valid_record() -> dict:
    """A representative valid frame."""
    return schema.record(
        t        = 12345.678,
        seq      = 42,
        lat_ms   = 18.5,
        valid    = True,
        eye_mm   = np.array([12.0, -35.0, 648.0]),
        gaze     = np.array([0.0, 0.0, -1.0]),
        head_rot = np.array([0.1, -0.2, 0.03]),
        conf     = 0.93,
    )


def test_valid_record_has_exactly_the_protocol_fields() -> None:
    """No extra keys, no missing keys, and the documented order."""
    rec = valid_record()
    assert tuple(rec) == schema.FIELDS
    schema.validate(rec)


def test_invalid_record_nulls_everything_optional() -> None:
    """`valid=false` carries `t`, `seq`, `lat_ms` and nulls the rest."""
    rec = schema.record(t=1.0, seq=7, lat_ms=9.0, valid=False)
    assert tuple(rec) == schema.FIELDS
    assert rec["valid"] is False
    for key in schema.NULLABLE:
        assert rec[key] is None
    schema.validate(rec)


def test_line_round_trip() -> None:
    """Encoding produces exactly one newline-terminated line that decodes back."""
    rec  = valid_record()
    line = schema.encode(rec)
    assert line.endswith(b"\n")
    assert line.count(b"\n") == 1
    assert schema.decode(line) == rec


def test_numpy_values_become_plain_json() -> None:
    """NumPy scalars must not leak into the record; `json.dumps` would choke."""
    rec = valid_record()
    assert all(isinstance(v, float) for v in rec["eye_mm"])
    json.dumps(rec)


def test_multiple_records_form_a_jsonl_stream() -> None:
    """Concatenated lines split cleanly, which is the whole point of the format."""
    blob = b"".join(schema.encode(schema.record(t=float(i), seq=i, lat_ms=1.0, valid=False))
                    for i in range(5))
    lines = blob.splitlines()
    assert len(lines) == 5
    assert [schema.decode(x)["seq"] for x in lines] == list(range(5))


def test_gaze_must_be_a_unit_vector() -> None:
    """A non-unit gaze vector is a bug in the producer, so validation rejects it."""
    rec = valid_record()
    rec["gaze"] = [0.0, 0.0, -2.0]
    with pytest.raises(ValueError, match="unit vector"):
        schema.validate(rec)


def test_extra_field_rejected() -> None:
    """Consumers parse a fixed schema; an unexpected field is an error here, not there."""
    rec = valid_record()
    rec["extra"] = 1
    with pytest.raises(ValueError, match="field mismatch"):
        schema.validate(rec)


def test_missing_field_rejected() -> None:
    """A dropped field is caught by name."""
    rec = valid_record()
    del rec["lat_ms"]
    with pytest.raises(ValueError, match="lat_ms"):
        schema.validate(rec)


@pytest.mark.parametrize("key", schema.NULLABLE)
def test_invalid_frame_must_not_carry_values(key: str) -> None:
    """Nulling is mandatory on invalid frames so consumers cannot read stale data."""
    rec = schema.record(t=1.0, seq=1, lat_ms=1.0, valid=False)
    rec[key] = [1.0, 2.0, 3.0] if key in schema.VECTORS else 1.0
    with pytest.raises(ValueError, match="must be null"):
        schema.validate(rec)


@pytest.mark.parametrize("key", schema.VECTORS)
def test_vectors_must_have_three_components(key: str) -> None:
    """Two- or four-element vectors are rejected."""
    rec = valid_record()
    rec[key] = [1.0, 2.0]
    with pytest.raises(ValueError, match="3-element"):
        schema.validate(rec)


def test_bools_are_not_accepted_as_numbers() -> None:
    """Python's `bool` is an `int`; the validator must not be fooled by that."""
    rec = valid_record()
    rec["seq"] = True
    with pytest.raises(ValueError, match="`seq` must be an int"):
        schema.validate(rec)


def test_nan_is_refused_at_encode_time() -> None:
    """`NaN` is not valid JSON; encoding must fail rather than emit it."""
    rec = valid_record()
    rec["lat_ms"] = math.nan
    with pytest.raises(ValueError):
        schema.encode(rec)


def test_short_vector_input_raises() -> None:
    """Building a record from a two-element vector is caught at construction."""
    with pytest.raises(ValueError, match="3 components"):
        schema.record(t=1.0, seq=1, lat_ms=1.0, valid=True,
                      eye_mm=[1.0, 2.0], gaze=[0, 0, -1], head_rot=[0, 0, 0], conf=1.0)


# --- additive fields (--estimator both) ---


def test_optional_estimator_fields_are_absent_by_default() -> None:
    """The common case is still exactly the eight documented keys."""
    assert tuple(valid_record()) == schema.FIELDS


def test_both_estimators_add_two_keys() -> None:
    """`--estimator both` adds `gaze_iris`/`gaze_l2cs` without disturbing `gaze`."""
    rec = schema.record(
        t=1.0, seq=1, lat_ms=2.0, valid=True,
        eye_mm=[0.0, 0.0, 600.0], gaze=[0.0, 0.0, -1.0], head_rot=[0.0, 0.0, 0.0],
        conf=0.9,
        gaze_iris=[0.1, 0.0, -math.sqrt(1 - 0.01)],
        gaze_l2cs=[0.0, 0.0, -1.0],
    )
    schema.validate(rec)
    assert set(rec) == set(schema.FIELDS) | {"gaze_iris", "gaze_l2cs"}
    assert schema.decode(schema.encode(rec)) == rec


def test_optional_fields_must_also_be_unit_vectors() -> None:
    """The extra estimators are held to the same contract as the primary one."""
    rec = valid_record()
    rec["gaze_iris"] = [0.0, 0.0, -3.0]
    with pytest.raises(ValueError, match="`gaze_iris` must be a unit vector"):
        schema.validate(rec)


def test_unknown_fields_are_still_rejected() -> None:
    """Additive does not mean anything goes; only the two are allowed."""
    rec = valid_record()
    rec["gaze_wild"] = [0.0, 0.0, -1.0]
    with pytest.raises(ValueError, match="field mismatch"):
        schema.validate(rec)


def test_invalid_frame_carries_no_estimator_fields() -> None:
    """An invalid frame nulls everything, including the additive keys."""
    rec = schema.record(t=1.0, seq=1, lat_ms=1.0, valid=False)
    rec["gaze_iris"] = [0.0, 0.0, -1.0]
    with pytest.raises(ValueError, match="must be absent or null"):
        schema.validate(rec)
