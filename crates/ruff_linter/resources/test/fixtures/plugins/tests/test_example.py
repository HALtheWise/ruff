import time

import pytest
from unittest import mock


def test_sleeps():
    time.sleep(1)
    assert True


def test_asserts_nothing():
    value = 1 + 1
    print(value)


def test_raises():
    with pytest.raises(ValueError):
        int("x")


def test_mock_assertion():
    m = mock.Mock()
    m()
    m.assert_called_once()


@pytest.mark.skip()
def test_skipped_without_reason():
    assert False


@pytest.mark.skip(reason="flaky on CI")
def test_skipped_with_reason():
    assert False


def helper():
    time.sleep(1)  # TST001: rules apply to the whole file
