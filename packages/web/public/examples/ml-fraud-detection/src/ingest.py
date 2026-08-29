from __future__ import annotations

from common import read_csv, write_csv

FIELDS = [
    "transaction_id",
    "amount",
    "hour",
    "country_risk",
    "card_present",
    "days_ago",
    "label",
    "split",
]


def main() -> None:
    normalized: list[dict[str, object]] = []
    for row in read_csv("data/transactions.csv"):
        missing = [field for field in FIELDS if not row.get(field)]
        if missing:
            transaction_id = row.get("transaction_id", "<unknown>")
            raise ValueError(f"{transaction_id}: missing {missing}")
        if row["split"] not in {"train", "test"}:
            raise ValueError(f"invalid split: {row['split']}")
        normalized.append(
            {
                "transaction_id": row["transaction_id"],
                "amount": f"{float(row['amount']):.2f}",
                "hour": int(row["hour"]),
                "country_risk": f"{float(row['country_risk']):.4f}",
                "card_present": int(row["card_present"]),
                "days_ago": int(row["days_ago"]),
                "label": int(row["label"]),
                "split": row["split"],
            }
        )
    normalized.sort(key=lambda row: str(row["transaction_id"]))
    write_csv("data/raw.csv", FIELDS, normalized)
    print(f"ingested {len(normalized)} transactions")


if __name__ == "__main__":
    main()
