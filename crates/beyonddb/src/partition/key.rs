//! Canonical partition ownership and ordered key encoding.

use bigdecimal::BigDecimal;
use extenddb_core::types::{AttributeValue, Item, KeySchemaElement, KeyType};

use crate::items::item_key;
use crate::{Error, Result};

pub(crate) fn index_key(item: &Item, schema: &[KeySchemaElement]) -> Result<(Vec<u8>, Vec<u8>)> {
    let partition = partition_key_bytes(item, schema)?;
    let mut sort = Vec::new();
    for element in schema
        .iter()
        .filter(|element| element.key_type == KeyType::Range)
    {
        let value = item
            .get(&element.attribute_name)
            .ok_or(Error::Command("missing sort key attribute"))?;
        sort.extend_from_slice(&sort_component(value)?);
    }
    Ok((partition, sort))
}

pub(crate) fn sort_component(value: &AttributeValue) -> Result<Vec<u8>> {
    let encoded = match value {
        AttributeValue::S(value) => value.as_bytes().to_vec(),
        AttributeValue::B(value) => value.clone(),
        AttributeValue::N(value) => orderable_number(value)?.into_bytes(),
        _ => return Err(Error::Command("invalid sort key type")),
    };
    let mut sort = escape_sort_bytes(&encoded);
    sort.extend_from_slice(&[0, 0]);
    Ok(sort)
}

pub(crate) fn sort_prefix(value: &AttributeValue) -> Result<Vec<u8>> {
    match value {
        AttributeValue::S(value) => Ok(escape_sort_bytes(value.as_bytes())),
        AttributeValue::B(value) => Ok(escape_sort_bytes(value)),
        _ => Err(Error::Command(
            "begins_with requires a string or binary sort key",
        )),
    }
}

fn escape_sort_bytes(encoded: &[u8]) -> Vec<u8> {
    let mut sort = Vec::with_capacity(encoded.len() + 2);
    for &byte in encoded {
        if byte == 0 {
            sort.extend_from_slice(&[0, 0xff]);
        } else {
            sort.push(byte);
        }
    }
    sort
}

pub(crate) fn partition_key_bytes(item: &Item, schema: &[KeySchemaElement]) -> Result<Vec<u8>> {
    let mut partition = Item::new();
    for element in schema
        .iter()
        .filter(|element| element.key_type == KeyType::Hash)
    {
        let value = item
            .get(&element.attribute_name)
            .ok_or(Error::Command("missing partition key attribute"))?;
        partition.insert(element.attribute_name.clone(), value.clone());
    }
    if partition.is_empty() {
        return Err(Error::Command("table has no partition key"));
    }
    item_key(&partition, schema)
}

/// Hashes only HASH attributes so every sort-key sibling has one Cell owner.
pub fn data_key_hash(table_id: &str, key: &Item, schema: &[KeySchemaElement]) -> Result<[u8; 16]> {
    let partition = partition_key_bytes(key, schema)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"beyonddb.item.partition.v1\0");
    hasher.update(table_id.as_bytes());
    hasher.update(&partition);
    let mut hash = [0_u8; 16];
    hash.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    Ok(hash)
}

fn orderable_number(value: &str) -> Result<String> {
    let number = value
        .parse::<BigDecimal>()
        .map_err(|_| Error::Command("invalid numeric sort key"))?;
    let zero = BigDecimal::from(0);
    if number == zero {
        return Ok("1".into());
    }
    let negative = number < zero;
    let magnitude = if negative { -number } else { number }.normalized();
    let (mantissa, scale) = magnitude.as_bigint_and_exponent();
    let digits = mantissa.to_string();
    let exponent = i64::try_from(digits.len())
        .map_err(|_| Error::Command("numeric sort key is too large"))?
        - scale;
    let biased = if negative {
        100_000 - exponent
    } else {
        100_000 + exponent
    };
    if !(0..1_000_000).contains(&biased) {
        return Err(Error::Command("numeric sort key exponent is out of range"));
    }
    if negative {
        let complement: String = digits
            .bytes()
            .map(|digit| char::from(b'9' - (digit - b'0')))
            .collect();
        Ok(format!("0{biased:06}{complement}:"))
    } else {
        Ok(format!("2{biased:06}{digits}"))
    }
}
