import structlog

log = structlog.get_logger()

log.info("payment_failed", order_id=1)  # OK
log.info(f"payment {1} failed")
log.warning("Payment failed for %s", 1)
log.error("PaymentFailed")
print("payment_failed")  # OK: not a logger
