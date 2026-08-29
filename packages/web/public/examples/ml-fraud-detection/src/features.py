from __future__ import annotations

import argparse
import math

from common import FEATURE_COLUMNS, read_csv, read_json, write_csv


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", default="params.json")
    args = parser.parse_args()
    lookback_days = int(read_json(args.config)["features"]["lookback_days"])

    featured: list[dict[str, object]] = []
    for row in read_csv("data/raw.csv"):
        if int(row["days_ago"]) > lookback_days:
            continue
        hour = int(row["hour"])
        featured.append(
            {
                "transaction_id": row["transaction_id"],
                "amount_log": f"{math.log1p(float(row['amount'])):.8f}",
                "night": int(hour < 6 or hour >= 22),
                "country_risk": f"{float(row['country_risk']):.8f}",
                "card_not_present": 1 - int(row["card_present"]),
                "label": int(row["label"]),
                "split": row["split"],
            }
        )

    fields = ["transaction_id", *FEATURE_COLUMNS, "label", "split"]
    write_csv("data/features.csv", fields, featured)
    print(f"created {len(featured)} feature rows")


if __name__ == "__main__":
    main()
