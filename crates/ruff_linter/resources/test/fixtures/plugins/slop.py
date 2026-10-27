import abc
from typing import Protocol, overload


# Step 1: load the configuration
def load_config():
    # This function loads the configuration from disk.
    return {}


def handle_event(event):
    """Handle an event."""
    ...


def todo():
    raise NotImplementedError


class Base(abc.ABC):
    def run(self):  # OK: in an ABC
        raise NotImplementedError

    @abc.abstractmethod
    def stop(self):  # OK
        pass


class Reader(Protocol):
    def read(self) -> bytes:  # OK: a protocol member
        ...


@overload
def parse(value: int) -> int: ...  # OK
def parse(value):
    return value


# Retry because the upstream API is flaky.  (OK: explains why)
def fetch():
    return load_config()
