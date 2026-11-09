#!/usr/bin/env python3
"""Offline Gemini cost estimate; no API calls or third-party dependencies.

Default paid text rates: https://ai.google.dev/gemini-api/docs/pricing#gemini-2.5-flash
Checked 2026-11-09. Token sizes and cache rates are assumptions, not telemetry.
"""
import argparse
from decimal import Decimal, InvalidOperation


def nonnegative(value):
    try:
        number = Decimal(value)
    except InvalidOperation as error:
        raise argparse.ArgumentTypeError("must be a number") from error
    if not number.is_finite() or number < 0:
        raise argparse.ArgumentTypeError("must be finite and nonnegative")
    return number


def percentage(value):
    number = nonnegative(value)
    if number > 100:
        raise argparse.ArgumentTypeError("must be between 0 and 100")
    return number


def estimate(errors, analyses, cache_percent, input_tokens, output_tokens, input_rate, output_rate):
    requests = errors * analyses * (1 - cache_percent / Decimal(100))
    per_request = (input_tokens * input_rate + output_tokens * output_rate) / Decimal(1_000_000)
    return requests, per_request, requests * per_request


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--errors", type=nonnegative, default=Decimal(1000), help="eligible errors in your chosen period (default: 1000)")
    parser.add_argument("--analyses-per-error", type=nonnegative, default=Decimal(1), help="analysis attempts per error, including repeats after edits (default: 1)")
    parser.add_argument("--cache-hit-rate", type=percentage, default=Decimal(0), help="percent of attempts served by the local cache (default: 0)")
    parser.add_argument("--input-tokens", type=nonnegative, default=Decimal(2000), help="average full prompt tokens per request (default: 2000)")
    parser.add_argument("--output-tokens", type=nonnegative, default=Decimal(600), help="average explanation + replacement tokens (default: 600)")
    parser.add_argument("--input-rate", type=nonnegative, default=Decimal("0.30"), help="USD per million input tokens (default: 0.30)")
    parser.add_argument("--output-rate", type=nonnegative, default=Decimal("2.50"), help="USD per million output tokens (default: 2.50)")
    args = parser.parse_args()
    requests, per_request, total = estimate(args.errors, args.analyses_per_error, args.cache_hit_rate,
                                           args.input_tokens, args.output_tokens, args.input_rate, args.output_rate)
    print("Rust Mentor · Gemini paid-tier estimate (USD)")
    print(f"Expected model requests: {requests:,.2f}")
    print(f"Cost per model request:  ${per_request:.6f}")
    print(f"Estimated total:         ${total:,.4f}")
    print("Excludes optional Jev charges, taxes, and local compute. Not measured usage.")


if __name__ == "__main__":
    main()
