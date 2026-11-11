/*
 * Copyright (C) 2026 Open Source Robotics Foundation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
*/

//! Converts between plain JSON and SiLA `Any` fields in gRPC messages.
//!
//! An `Any` is an XML type description plus packed protobuf bytes.
//! prost-reflect can only fill those bytes from base64.
//!
//! The packer writes a JSON object as a list of `{Key, Value}` structures,
//! because SiLA has no map type. A server must use the same rule to read the
//! object back.

use base64::Engine;
use prost::encoding::{WireType, decode_key, decode_varint, encode_key, encode_varint};
use prost_reflect::{Kind, MessageDescriptor};
use roxmltree::{Document, Node};
use serde_json::Value;
use thiserror::Error as ThisError;
use tracing::warn;

const ANY_FULL_NAME: &str = "sila2.org.silastandard.Any";

// These match the normalised XML of sila2 byte for byte. The server compares
// the Void string exactly. The encoding test checks all of these constants.
const STRING_XML: &str =
    r#"<DataType xmlns="http://www.sila-standard.org"><Basic>String</Basic></DataType>"#;
const INTEGER_XML: &str =
    r#"<DataType xmlns="http://www.sila-standard.org"><Basic>Integer</Basic></DataType>"#;
const REAL_XML: &str =
    r#"<DataType xmlns="http://www.sila-standard.org"><Basic>Real</Basic></DataType>"#;
const BOOLEAN_XML: &str =
    r#"<DataType xmlns="http://www.sila-standard.org"><Basic>Boolean</Basic></DataType>"#;
const VOID_XML: &str = r#"<DataType xmlns="http://www.sila-standard.org"><Constrained><DataType><Basic>String</Basic></DataType><Constraints><Length>0</Length></Constraints></Constrained></DataType>"#;
const LIST_XML: &str = r#"<DataType xmlns="http://www.sila-standard.org"><List><DataType><Basic>Any</Basic></DataType></List></DataType>"#;
const DICT_XML: &str = r#"<DataType xmlns="http://www.sila-standard.org"><List><DataType><Structure><Element><Identifier>Key</Identifier><DisplayName>Key</DisplayName><Description>Map key</Description><DataType><Basic>String</Basic></DataType></Element><Element><Identifier>Value</Identifier><DisplayName>Value</DisplayName><Description>Map value</Description><DataType><Basic>Any</Basic></DataType></Element></Structure></DataType></List></DataType>"#;

/// Skips a value that already has the shape `{type, payload}`.
pub(crate) fn pack_any_fields(descriptor: &MessageDescriptor, value: &mut Value) {
    walk_any_fields(descriptor, value, &pack_any_value);
}

/// Keeps an `Any` as `{type, payload}` when it does not decode.
pub(crate) fn unpack_any_fields(descriptor: &MessageDescriptor, value: &mut Value) {
    walk_any_fields(descriptor, value, &unpack_any_value);
}

fn pack_any_value(value: &mut Value) {
    if is_prepacked(value) {
        return;
    }
    let packed = encode(value);
    *value = serde_json::json!({
        "type": packed.type_xml,
        "payload": base64::engine::general_purpose::STANDARD.encode(packed.payload),
    });
}

fn unpack_any_value(value: &mut Value) {
    if !is_prepacked(value) {
        return;
    }
    match decode_prepacked(value) {
        Ok(decoded) => *value = decoded,
        Err(error) => warn!("Leaving a SiLA Any undecoded: {error}"),
    }
}

fn decode_prepacked(value: &Value) -> Result<Value, DecodeError> {
    let payload_base64 = value["payload"].as_str().unwrap_or_default();
    let payload = base64::engine::general_purpose::STANDARD
        .decode(payload_base64)
        .map_err(|_| DecodeError::MalformedPayload)?;
    let type_xml = value["type"].as_str().unwrap_or_default();
    decode(type_xml, &payload)
}

/// Skips unknown fields, so that prost-reflect reports them later.
fn walk_any_fields(
    descriptor: &MessageDescriptor,
    value: &mut Value,
    on_any: &impl Fn(&mut Value),
) {
    let Value::Object(fields) = value else {
        return;
    };
    for (name, field_value) in fields.iter_mut() {
        let Some(field) = descriptor
            .get_field_by_name(name)
            .or_else(|| descriptor.get_field_by_json_name(name))
        else {
            continue;
        };

        if field.is_map() {
            let Kind::Message(entry) = field.kind() else {
                continue;
            };
            let Kind::Message(value_type) = entry.map_entry_value_field().kind() else {
                continue;
            };
            if let Value::Object(map) = field_value {
                for item in map.values_mut() {
                    walk_message(&value_type, item, on_any);
                }
            }
            continue;
        }

        let Kind::Message(message_type) = field.kind() else {
            continue;
        };
        if field.is_list() {
            if let Value::Array(items) = field_value {
                for item in items {
                    walk_message(&message_type, item, on_any);
                }
            }
        } else {
            walk_message(&message_type, field_value, on_any);
        }
    }
}

fn walk_message(descriptor: &MessageDescriptor, value: &mut Value, on_any: &impl Fn(&mut Value)) {
    if descriptor.full_name() == ANY_FULL_NAME {
        on_any(value);
    } else {
        walk_any_fields(descriptor, value, on_any);
    }
}

fn is_prepacked(value: &Value) -> bool {
    let Value::Object(object) = value else {
        return false;
    };
    object.len() == 2
        && object.get("type").is_some_and(Value::is_string)
        && object.get("payload").is_some_and(Value::is_string)
}

/// The payload holds raw protobuf bytes, not base64.
struct PackedAny {
    type_xml: &'static str,
    payload: Vec<u8>,
}

/// SiLA `Integer` is `int64`, so a larger integer goes as `Real` and can lose
/// precision.
fn encode(value: &Value) -> PackedAny {
    match value {
        Value::Null => PackedAny {
            type_xml: VOID_XML,
            payload: wrap_in_holder(&[]),
        },
        Value::Bool(boolean) => PackedAny {
            type_xml: BOOLEAN_XML,
            payload: wrap_in_holder(&encode_bool_field(*boolean)),
        },
        Value::Number(number) => match number.as_i64() {
            Some(integer) => PackedAny {
                type_xml: INTEGER_XML,
                payload: wrap_in_holder(&encode_int64_field(integer)),
            },
            None => {
                let real = number.as_f64().unwrap_or(0.0);
                PackedAny {
                    type_xml: REAL_XML,
                    payload: wrap_in_holder(&encode_double_field(real)),
                }
            }
        },
        Value::String(text) => PackedAny {
            type_xml: STRING_XML,
            payload: wrap_in_holder(&encode_bytes_field(1, text.as_bytes())),
        },
        Value::Array(items) => {
            let mut buffer = Vec::new();
            for item in items {
                encode_message_field(1, &encode_any(item), &mut buffer);
            }
            PackedAny {
                type_xml: LIST_XML,
                payload: buffer,
            }
        }
        Value::Object(object) => {
            let mut buffer = Vec::new();
            for (key, item) in object {
                let mut entry = Vec::new();
                encode_message_field(1, &encode_bytes_field(1, key.as_bytes()), &mut entry);
                encode_message_field(2, &encode_any(item), &mut entry);
                encode_message_field(1, &entry, &mut buffer);
            }
            PackedAny {
                type_xml: DICT_XML,
                payload: buffer,
            }
        }
    }
}

/// `Any { string type = 1; bytes payload = 2; }`
fn encode_any(value: &Value) -> Vec<u8> {
    let packed = encode(value);
    let mut buffer = encode_bytes_field(1, packed.type_xml.as_bytes());
    buffer.extend(encode_bytes_field(2, &packed.payload));
    buffer
}

/// sila2 wraps every Any payload in a one-field holder message, so a basic
/// value sits at field 1 even when it packs to nothing.
fn wrap_in_holder(inner: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::new();
    encode_message_field(1, inner, &mut buffer);
    buffer
}

// proto3 does not write a scalar field that holds its default value.

fn encode_message_field(tag: u32, inner: &[u8], buffer: &mut Vec<u8>) {
    encode_key(tag, WireType::LengthDelimited, buffer);
    encode_varint(inner.len() as u64, buffer);
    buffer.extend_from_slice(inner);
}

fn encode_bytes_field(tag: u32, bytes: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::new();
    if !bytes.is_empty() {
        encode_message_field(tag, bytes, &mut buffer);
    }
    buffer
}

fn encode_bool_field(value: bool) -> Vec<u8> {
    let mut buffer = Vec::new();
    if value {
        encode_key(1, WireType::Varint, &mut buffer);
        encode_varint(1, &mut buffer);
    }
    buffer
}

fn encode_int64_field(value: i64) -> Vec<u8> {
    let mut buffer = Vec::new();
    if value != 0 {
        encode_key(1, WireType::Varint, &mut buffer);
        encode_varint(value as u64, &mut buffer);
    }
    buffer
}

fn encode_double_field(value: f64) -> Vec<u8> {
    let mut buffer = Vec::new();
    if value != 0.0 {
        encode_key(1, WireType::SixtyFourBit, &mut buffer);
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    buffer
}

/// Omits the SiLA types Date, Time and Timestamp.
#[derive(Debug, Clone, PartialEq)]
enum SilaType {
    Basic(Basic),
    List(Box<SilaType>),
    Structure(Vec<Element>),
}

/// Its position in the Structure sets its protobuf field number.
#[derive(Debug, Clone, PartialEq)]
struct Element {
    name: String,
    data_type: SilaType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basic {
    String,
    Integer,
    Real,
    Boolean,
    Binary,
    Any,
}

#[derive(Debug, PartialEq, ThisError)]
enum DecodeError {
    #[error("the type XML is not supported")]
    UnsupportedType,
    #[error("the payload is not valid protobuf")]
    MalformedPayload,
    #[error("a String is not valid UTF-8")]
    InvalidUtf8,
    #[error("a Real has no JSON form, such as NaN or Infinity")]
    NonFiniteReal,
}

fn decode(type_xml: &str, payload: &[u8]) -> Result<Value, DecodeError> {
    if type_xml == VOID_XML {
        return Ok(Value::Null);
    }
    let sila_type = parse_type(type_xml).ok_or(DecodeError::UnsupportedType)?;
    let fields = read_fields(payload).ok_or(DecodeError::MalformedPayload)?;
    read_value(&sila_type, &fields, 1)
}

/// Tags match by local name, so the SiLA namespace is optional.
fn parse_type(xml: &str) -> Option<SilaType> {
    let document = Document::parse(xml).ok()?;
    parse_data_type(document.root_element())
}

fn find_child_elements<'tree, 'input>(node: Node<'tree, 'input>) -> Vec<Node<'tree, 'input>> {
    node.children().filter(|child| child.is_element()).collect()
}

fn is_named(node: &Node<'_, '_>, name: &str) -> bool {
    node.tag_name().name() == name
}

fn parse_data_type(node: Node<'_, '_>) -> Option<SilaType> {
    if !is_named(&node, "DataType") {
        return None;
    }
    let children = find_child_elements(node);
    let [body] = children.as_slice() else {
        return None;
    };
    match body.tag_name().name() {
        "Basic" => {
            let basic = match body.text()? {
                "String" => Basic::String,
                "Integer" => Basic::Integer,
                "Real" => Basic::Real,
                "Boolean" => Basic::Boolean,
                "Binary" => Basic::Binary,
                "Any" => Basic::Any,
                _ => return None,
            };
            Some(SilaType::Basic(basic))
        }
        "List" => {
            let children = find_child_elements(*body);
            let [item_node] = children.as_slice() else {
                return None;
            };
            let item = parse_data_type(*item_node)?;
            Some(SilaType::List(Box::new(item)))
        }
        "Structure" => {
            let mut elements = Vec::new();
            for child in find_child_elements(*body) {
                elements.push(parse_element(child)?);
            }
            Some(SilaType::Structure(elements))
        }
        // Constraints limit the allowed values. The wire format is the same as
        // the inner DataType.
        "Constrained" => {
            let children = find_child_elements(*body);
            let [inner_node, rest @ ..] = children.as_slice() else {
                return None;
            };
            let has_only_constraints = rest.iter().all(|child| is_named(child, "Constraints"));
            if !has_only_constraints {
                return None;
            }
            parse_data_type(*inner_node)
        }
        _ => None,
    }
}

fn parse_element(node: Node<'_, '_>) -> Option<Element> {
    if !is_named(&node, "Element") {
        return None;
    }
    let children = find_child_elements(node);
    let [identifier, .., type_node] = children.as_slice() else {
        return None;
    };
    if !is_named(identifier, "Identifier") {
        return None;
    }
    let name = identifier.text()?.to_owned();
    let data_type = parse_data_type(*type_node)?;
    Some(Element { name, data_type })
}

fn is_map_entry(sila_type: &SilaType) -> bool {
    let SilaType::Structure(elements) = sila_type else {
        return false;
    };
    let [key, value] = elements.as_slice() else {
        return false;
    };
    key.name == "Key"
        && value.name == "Value"
        && key.data_type == SilaType::Basic(Basic::String)
        && value.data_type == SilaType::Basic(Basic::Any)
}

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64([u8; 8]),
}

/// Each field paired with its protobuf tag.
type Fields<'a> = Vec<(u32, Field<'a>)>;

fn read_fields(bytes: &[u8]) -> Option<Fields<'_>> {
    let mut rest = bytes;
    let mut fields = Vec::new();
    while !rest.is_empty() {
        let (tag, wire_type) = decode_key(&mut rest).ok()?;
        let field = match wire_type {
            WireType::Varint => {
                let varint = decode_varint(&mut rest).ok()?;
                Field::Varint(varint)
            }
            WireType::SixtyFourBit => {
                let slice = rest.get(..8)?;
                let fixed64: [u8; 8] = slice.try_into().ok()?;
                rest = &rest[8..];
                Field::Fixed64(fixed64)
            }
            WireType::LengthDelimited => {
                let length = decode_varint(&mut rest).ok()?;
                let length = usize::try_from(length).ok()?;
                let field_bytes = rest.get(..length)?;
                rest = &rest[length..];
                Field::Bytes(field_bytes)
            }
            WireType::ThirtyTwoBit => {
                if rest.len() < 4 {
                    return None;
                }
                rest = &rest[4..];
                continue;
            }
            WireType::StartGroup | WireType::EndGroup => return None,
        };
        fields.push((tag, field));
    }
    Some(fields)
}

/// Protobuf merges a repeated nested message, but SiLA servers do not send one.
fn find_last_field<'list, 'bytes>(
    fields: &'list Fields<'bytes>,
    tag: u32,
) -> Option<&'list Field<'bytes>> {
    let newest_first = fields.iter().rev();
    for (field_tag, field) in newest_first {
        if *field_tag == tag {
            return Some(field);
        }
    }
    None
}

fn find_bytes<'a>(fields: &Fields<'a>, tag: u32) -> Result<Option<&'a [u8]>, DecodeError> {
    match find_last_field(fields, tag) {
        Some(Field::Bytes(bytes)) => Ok(Some(bytes)),
        Some(_) => Err(DecodeError::MalformedPayload),
        None => Ok(None),
    }
}

fn find_text<'a>(fields: &Fields<'a>, tag: u32) -> Result<&'a str, DecodeError> {
    let bytes = find_bytes(fields, tag)?.unwrap_or_default();
    std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)
}

fn read_value(sila_type: &SilaType, fields: &Fields<'_>, tag: u32) -> Result<Value, DecodeError> {
    if let SilaType::List(item) = sila_type {
        let mut items = Vec::new();
        for (field_tag, field) in fields {
            if *field_tag != tag {
                continue;
            }
            let Field::Bytes(bytes) = field else {
                return Err(DecodeError::MalformedPayload);
            };
            items.push(read_message(item, bytes)?);
        }

        if !is_map_entry(item) {
            return Ok(Value::Array(items));
        }
        let mut object = serde_json::Map::new();
        for entry in items {
            if let Some((key, value)) = as_entry(entry) {
                object.insert(key, value);
            }
        }
        return Ok(Value::Object(object));
    }
    match find_bytes(fields, tag)? {
        Some(bytes) => read_message(sila_type, bytes),
        None => make_default(sila_type),
    }
}

fn as_entry(entry: Value) -> Option<(String, Value)> {
    let Value::Object(mut fields) = entry else {
        return None;
    };
    let key = fields.remove("Key")?;
    let value = fields.remove("Value")?;
    Some((key.as_str()?.to_owned(), value))
}

fn read_message(sila_type: &SilaType, bytes: &[u8]) -> Result<Value, DecodeError> {
    let fields = read_fields(bytes).ok_or(DecodeError::MalformedPayload)?;
    match sila_type {
        SilaType::Structure(elements) => {
            let mut object = serde_json::Map::new();
            for (index, element) in elements.iter().enumerate() {
                let tag = u32::try_from(index + 1).map_err(|_| DecodeError::UnsupportedType)?;
                let value = read_value(&element.data_type, &fields, tag)?;
                object.insert(element.name.clone(), value);
            }
            Ok(Value::Object(object))
        }
        SilaType::Basic(Basic::Any) => {
            let type_xml = find_text(&fields, 1)?;
            let payload = find_bytes(&fields, 2)?.unwrap_or_default();
            decode(type_xml, payload)
        }
        SilaType::Basic(basic) => Ok(match basic {
            Basic::String => Value::String(find_text(&fields, 1)?.to_owned()),
            Basic::Binary => {
                let bytes = find_bytes(&fields, 1)?.unwrap_or_default();
                Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            Basic::Integer => match find_last_field(&fields, 1) {
                Some(Field::Varint(varint)) => Value::Number((*varint as i64).into()),
                Some(_) => return Err(DecodeError::MalformedPayload),
                None => Value::Number(0.into()),
            },
            Basic::Boolean => match find_last_field(&fields, 1) {
                Some(Field::Varint(varint)) => Value::Bool(*varint != 0),
                Some(_) => return Err(DecodeError::MalformedPayload),
                None => Value::Bool(false),
            },
            Basic::Real => match find_last_field(&fields, 1) {
                Some(Field::Fixed64(bytes)) => {
                    let real = f64::from_le_bytes(*bytes);
                    convert_real_to_json(real)?
                }
                Some(_) => return Err(DecodeError::MalformedPayload),
                None => convert_real_to_json(0.0)?,
            },
            Basic::Any => unreachable!("handled above"),
        }),
        SilaType::List(_) => Err(DecodeError::UnsupportedType),
    }
}

fn convert_real_to_json(real: f64) -> Result<Value, DecodeError> {
    serde_json::Number::from_f64(real)
        .map(Value::Number)
        .ok_or(DecodeError::NonFiniteReal)
}

/// proto3 does not write a field that holds its default value.
fn make_default(sila_type: &SilaType) -> Result<Value, DecodeError> {
    Ok(match sila_type {
        SilaType::List(_) => Value::Array(Vec::new()),
        SilaType::Structure(_) => read_message(sila_type, &[])?,
        SilaType::Basic(Basic::String | Basic::Binary) => Value::String(String::new()),
        SilaType::Basic(Basic::Integer) => Value::Number(0.into()),
        SilaType::Basic(Basic::Real) => convert_real_to_json(0.0)?,
        SilaType::Basic(Basic::Boolean) => Value::Bool(false),
        SilaType::Basic(Basic::Any) => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Diagram, JsonMessage, diagram::testing::DiagramTestFixture};
    use prost_reflect::{DescriptorPool, DynamicMessage};
    use protos::{
        AnyHolder,
        any_echo_server::{AnyEcho, AnyEchoServer},
    };
    use serde_json::json;
    use std::{sync::Arc, time::Duration};
    use tokio::runtime::Runtime;
    use tonic::{Request, Response, Status, transport::Server};

    mod protos {
        tonic::include_proto!("sila2.org.silastandard");
    }

    /// Each case is a plain value, its type XML and its payload. The payloads
    /// come from the sila2 Python library 0.14.0, with object keys sorted to
    /// match the `Map` of serde_json. Each payload holds the type XML of its
    /// inner values, so these cases also check the XML constants.
    fn make_encode_cases() -> Vec<(Value, &'static str, &'static str)> {
        vec![
            (json!("hello"), STRING_XML, "CgcKBWhlbGxv"),
            (json!(""), STRING_XML, "CgA="),
            (json!(42), INTEGER_XML, "CgIIKg=="),
            (json!(-7), INTEGER_XML, "CgsI+f//////////AQ=="),
            (json!(0), INTEGER_XML, "CgA="),
            (json!(1099511627776_i64), INTEGER_XML, "CgcIgICAgIAg"),
            (json!(1.5), REAL_XML, "CgkJAAAAAAAA+D8="),
            (json!(0.0), REAL_XML, "CgA="),
            (json!(-2.25), REAL_XML, "CgkJAAAAAAAAAsA="),
            (json!(true), BOOLEAN_XML, "CgIIAQ=="),
            (json!(false), BOOLEAN_XML, "CgA="),
            (json!(null), VOID_XML, "CgA="),
            (json!({}), DICT_XML, ""),
            (json!([]), LIST_XML, ""),
            (
                json!({"a": "b"}),
                DICT_XML,
                "Cl8KAwoBYRJYCk88RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5TdHJpbmc8L0Jhc2ljPjwvRGF0YVR5cGU+EgUKAwoBYg==",
            ),
            (
                json!({"n": 1, "s": "x"}),
                DICT_XML,
                "Cl8KAwoBbhJYClA8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5JbnRlZ2VyPC9CYXNpYz48L0RhdGFUeXBlPhIECgIIAQpfCgMKAXMSWApPPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48QmFzaWM+U3RyaW5nPC9CYXNpYz48L0RhdGFUeXBlPhIFCgMKAXg=",
            ),
            (
                json!({"a": {}}),
                DICT_XML,
                "CrMDCgMKAWESqwMKqAM8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxMaXN0PjxEYXRhVHlwZT48U3RydWN0dXJlPjxFbGVtZW50PjxJZGVudGlmaWVyPktleTwvSWRlbnRpZmllcj48RGlzcGxheU5hbWU+S2V5PC9EaXNwbGF5TmFtZT48RGVzY3JpcHRpb24+TWFwIGtleTwvRGVzY3JpcHRpb24+PERhdGFUeXBlPjxCYXNpYz5TdHJpbmc8L0Jhc2ljPjwvRGF0YVR5cGU+PC9FbGVtZW50PjxFbGVtZW50PjxJZGVudGlmaWVyPlZhbHVlPC9JZGVudGlmaWVyPjxEaXNwbGF5TmFtZT5WYWx1ZTwvRGlzcGxheU5hbWU+PERlc2NyaXB0aW9uPk1hcCB2YWx1ZTwvRGVzY3JpcHRpb24+PERhdGFUeXBlPjxCYXNpYz5Bbnk8L0Jhc2ljPjwvRGF0YVR5cGU+PC9FbGVtZW50PjwvU3RydWN0dXJlPjwvRGF0YVR5cGU+PC9MaXN0PjwvRGF0YVR5cGU+",
            ),
            (
                json!({"a": []}),
                DICT_XML,
                "CncKAwoBYRJwCm48RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxMaXN0PjxEYXRhVHlwZT48QmFzaWM+QW55PC9CYXNpYz48L0RhdGFUeXBlPjwvTGlzdD48L0RhdGFUeXBlPg==",
            ),
            (
                json!({"a": 1.5}),
                DICT_XML,
                "CmMKAwoBYRJcCk08RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5SZWFsPC9CYXNpYz48L0RhdGFUeXBlPhILCgkJAAAAAAAA+D8=",
            ),
            (
                json!({"a": 0}),
                DICT_XML,
                "Cl0KAwoBYRJWClA8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5JbnRlZ2VyPC9CYXNpYz48L0RhdGFUeXBlPhICCgA=",
            ),
            (
                json!({"a": false}),
                DICT_XML,
                "Cl0KAwoBYRJWClA8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5Cb29sZWFuPC9CYXNpYz48L0RhdGFUeXBlPhICCgA=",
            ),
            (
                json!({"a": null}),
                DICT_XML,
                "CrsBCgMKAWESswEKrAE8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxDb25zdHJhaW5lZD48RGF0YVR5cGU+PEJhc2ljPlN0cmluZzwvQmFzaWM+PC9EYXRhVHlwZT48Q29uc3RyYWludHM+PExlbmd0aD4wPC9MZW5ndGg+PC9Db25zdHJhaW50cz48L0NvbnN0cmFpbmVkPjwvRGF0YVR5cGU+EgIKAA==",
            ),
            (
                json!({"": ""}),
                DICT_XML,
                "ClkKABJVCk88RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5TdHJpbmc8L0Jhc2ljPjwvRGF0YVR5cGU+EgIKAA==",
            ),
            (
                json!([[]]),
                LIST_XML,
                "CnAKbjxEYXRhVHlwZSB4bWxucz0iaHR0cDovL3d3dy5zaWxhLXN0YW5kYXJkLm9yZyI+PExpc3Q+PERhdGFUeXBlPjxCYXNpYz5Bbnk8L0Jhc2ljPjwvRGF0YVR5cGU+PC9MaXN0PjwvRGF0YVR5cGU+",
            ),
            (
                json!([1, "two", true]),
                LIST_XML,
                "ClgKUDxEYXRhVHlwZSB4bWxucz0iaHR0cDovL3d3dy5zaWxhLXN0YW5kYXJkLm9yZyI+PEJhc2ljPkludGVnZXI8L0Jhc2ljPjwvRGF0YVR5cGU+EgQKAggBCloKTzxEYXRhVHlwZSB4bWxucz0iaHR0cDovL3d3dy5zaWxhLXN0YW5kYXJkLm9yZyI+PEJhc2ljPlN0cmluZzwvQmFzaWM+PC9EYXRhVHlwZT4SBwoFCgN0d28KWApQPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48QmFzaWM+Qm9vbGVhbjwvQmFzaWM+PC9EYXRhVHlwZT4SBAoCCAE=",
            ),
            (
                json!({"outer": {"inner": [1, {"k": null}]}}),
                DICT_XML,
                "CoUKCgcKBW91dGVyEvkJCqgDPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48TGlzdD48RGF0YVR5cGU+PFN0cnVjdHVyZT48RWxlbWVudD48SWRlbnRpZmllcj5LZXk8L0lkZW50aWZpZXI+PERpc3BsYXlOYW1lPktleTwvRGlzcGxheU5hbWU+PERlc2NyaXB0aW9uPk1hcCBrZXk8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+U3RyaW5nPC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48RWxlbWVudD48SWRlbnRpZmllcj5WYWx1ZTwvSWRlbnRpZmllcj48RGlzcGxheU5hbWU+VmFsdWU8L0Rpc3BsYXlOYW1lPjxEZXNjcmlwdGlvbj5NYXAgdmFsdWU8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+QW55PC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48L1N0cnVjdHVyZT48L0RhdGFUeXBlPjwvTGlzdD48L0RhdGFUeXBlPhLLBgrIBgoHCgVpbm5lchK8BgpuPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48TGlzdD48RGF0YVR5cGU+PEJhc2ljPkFueTwvQmFzaWM+PC9EYXRhVHlwZT48L0xpc3Q+PC9EYXRhVHlwZT4SyQUKWApQPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48QmFzaWM+SW50ZWdlcjwvQmFzaWM+PC9EYXRhVHlwZT4SBAoCCAEK7AQKqAM8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxMaXN0PjxEYXRhVHlwZT48U3RydWN0dXJlPjxFbGVtZW50PjxJZGVudGlmaWVyPktleTwvSWRlbnRpZmllcj48RGlzcGxheU5hbWU+S2V5PC9EaXNwbGF5TmFtZT48RGVzY3JpcHRpb24+TWFwIGtleTwvRGVzY3JpcHRpb24+PERhdGFUeXBlPjxCYXNpYz5TdHJpbmc8L0Jhc2ljPjwvRGF0YVR5cGU+PC9FbGVtZW50PjxFbGVtZW50PjxJZGVudGlmaWVyPlZhbHVlPC9JZGVudGlmaWVyPjxEaXNwbGF5TmFtZT5WYWx1ZTwvRGlzcGxheU5hbWU+PERlc2NyaXB0aW9uPk1hcCB2YWx1ZTwvRGVzY3JpcHRpb24+PERhdGFUeXBlPjxCYXNpYz5Bbnk8L0Jhc2ljPjwvRGF0YVR5cGU+PC9FbGVtZW50PjwvU3RydWN0dXJlPjwvRGF0YVR5cGU+PC9MaXN0PjwvRGF0YVR5cGU+Er4BCrsBCgMKAWsSswEKrAE8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxDb25zdHJhaW5lZD48RGF0YVR5cGU+PEJhc2ljPlN0cmluZzwvQmFzaWM+PC9EYXRhVHlwZT48Q29uc3RyYWludHM+PExlbmd0aD4wPC9MZW5ndGg+PC9Db25zdHJhaW50cz48L0NvbnN0cmFpbmVkPjwvRGF0YVR5cGU+EgIKAA==",
            ),
            (json!("tëst ✓"), STRING_XML, "CgsKCXTDq3N0IOKckw=="),
        ]
    }

    fn load_descriptor_pool() -> DescriptorPool {
        DescriptorPool::decode(
            &include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin"))[..],
        )
        .unwrap()
    }

    fn encode_base64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn test_encoding_matches_sila2_byte_for_byte() {
        for (input, expected_type, expected_payload) in make_encode_cases() {
            let packed = encode(&input);
            assert_eq!(packed.type_xml, expected_type, "type of {input}");
            assert_eq!(
                encode_base64(&packed.payload),
                expected_payload,
                "payload of {input}"
            );
        }
    }

    #[test]
    fn test_prepacked_values_pass_through_untouched() {
        let any_holder = load_descriptor_pool()
            .get_message_by_name("sila2.org.silastandard.AnyHolder")
            .unwrap();
        let prepacked = json!({ "type": "<DataType/>", "payload": "CgA=" });
        let mut message = json!({ "payload": prepacked.clone() });
        pack_any_fields(&any_holder, &mut message);
        assert_eq!(message["payload"], prepacked);

        let mut message = json!({ "payload": { "type": "x", "other": "y" } });
        pack_any_fields(&any_holder, &mut message);
        assert_eq!(message["payload"]["type"], DICT_XML);
    }

    #[test]
    fn test_unknown_fields_are_left_for_prost_reflect_to_reject() {
        let any_holder = load_descriptor_pool()
            .get_message_by_name("sila2.org.silastandard.AnyHolder")
            .unwrap();
        let mut message = json!({ "nonsense": { "a": 1 } });
        pack_any_fields(&any_holder, &mut message);
        assert_eq!(message, json!({ "nonsense": { "a": 1 } }));
        assert!(DynamicMessage::deserialize(any_holder, message).is_err());
    }

    fn make_basic_xml(name: &str) -> String {
        format!("<DataType><Basic>{name}</Basic></DataType>")
    }

    fn make_list_xml(item: &str) -> String {
        format!("<DataType><List>{item}</List></DataType>")
    }

    fn make_structure_xml(elements: &[(&str, String)]) -> String {
        let elements_xml: String = elements
            .iter()
            .map(|(name, data_type)| {
                format!(
                    "<Element><Identifier>{name}</Identifier><DisplayName>{name}</DisplayName>\
                     <Description>{name}</Description>{data_type}</Element>"
                )
            })
            .collect();
        format!("<DataType><Structure>{elements_xml}</Structure></DataType>")
    }

    /// The payloads come from the sila2 Python library 0.14.0. The packer does
    /// not write named Structures, but SiLA servers send them.
    #[test]
    fn test_decoding_matches_sila2_byte_for_byte() {
        let cases = [
            (
                make_structure_xml(&[("Name", make_basic_xml("String"))]),
                "CgkKBwoFaGVsbG8=",
                json!({"Name": "hello"}),
            ),
            (
                make_structure_xml(&[
                    ("Name", make_basic_xml("String")),
                    ("Count", make_basic_xml("Integer")),
                    ("Ok", make_basic_xml("Boolean")),
                    ("Ratio", make_basic_xml("Real")),
                ]),
                "ChgKAwoBeBICCAMaAggBIgkJAAAAAAAA+D8=",
                json!({"Count": 3, "Name": "x", "Ok": true, "Ratio": 1.5}),
            ),
            (
                make_structure_xml(&[
                    ("Name", make_basic_xml("String")),
                    ("Count", make_basic_xml("Integer")),
                    ("Ok", make_basic_xml("Boolean")),
                ]),
                "CgYKABIAGgA=",
                json!({"Count": 0, "Name": "", "Ok": false}),
            ),
            (
                make_structure_xml(&[(
                    "Child",
                    make_structure_xml(&[("Deep", make_basic_xml("Integer"))]),
                )]),
                "CgYKBAoCCAc=",
                json!({"Child": {"Deep": 7}}),
            ),
            (
                make_structure_xml(&[("Names", make_list_xml(&make_basic_xml("String")))]),
                "CgoKAwoBYQoDCgFi",
                json!({"Names": ["a", "b"]}),
            ),
            (
                make_structure_xml(&[("Names", make_list_xml(&make_basic_xml("String")))]),
                "CgA=",
                json!({"Names": []}),
            ),
            (
                make_list_xml(&make_structure_xml(&[("K", make_basic_xml("String"))])),
                "CgUKAwoBYQoFCgMKAWI=",
                json!([{"K": "a"}, {"K": "b"}]),
            ),
            (
                make_structure_xml(&[("Body", make_basic_xml("Any"))]),
                "Cl8KXQpPPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48QmFzaWM+U3RyaW5nPC9CYXNpYz48L0RhdGFUeXBlPhIKCggKBmluc2lkZQ==",
                json!({"Body": "inside"}),
            ),
            (
                make_structure_xml(&[("Body", make_basic_xml("Any"))]),
                "CpoKCpcKCqgDPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48TGlzdD48RGF0YVR5cGU+PFN0cnVjdHVyZT48RWxlbWVudD48SWRlbnRpZmllcj5LZXk8L0lkZW50aWZpZXI+PERpc3BsYXlOYW1lPktleTwvRGlzcGxheU5hbWU+PERlc2NyaXB0aW9uPk1hcCBrZXk8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+U3RyaW5nPC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48RWxlbWVudD48SWRlbnRpZmllcj5WYWx1ZTwvSWRlbnRpZmllcj48RGlzcGxheU5hbWU+VmFsdWU8L0Rpc3BsYXlOYW1lPjxEZXNjcmlwdGlvbj5NYXAgdmFsdWU8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+QW55PC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48L1N0cnVjdHVyZT48L0RhdGFUeXBlPjwvTGlzdD48L0RhdGFUeXBlPhLpBgr/BQoGCgRkYXRhEvQFCqgDPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48TGlzdD48RGF0YVR5cGU+PFN0cnVjdHVyZT48RWxlbWVudD48SWRlbnRpZmllcj5LZXk8L0lkZW50aWZpZXI+PERpc3BsYXlOYW1lPktleTwvRGlzcGxheU5hbWU+PERlc2NyaXB0aW9uPk1hcCBrZXk8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+U3RyaW5nPC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48RWxlbWVudD48SWRlbnRpZmllcj5WYWx1ZTwvSWRlbnRpZmllcj48RGlzcGxheU5hbWU+VmFsdWU8L0Rpc3BsYXlOYW1lPjxEZXNjcmlwdGlvbj5NYXAgdmFsdWU8L0Rlc2NyaXB0aW9uPjxEYXRhVHlwZT48QmFzaWM+QW55PC9CYXNpYz48L0RhdGFUeXBlPjwvRWxlbWVudD48L1N0cnVjdHVyZT48L0RhdGFUeXBlPjwvTGlzdD48L0RhdGFUeXBlPhLGAgreAQoKCghhZGRlZElkcxLPAQpuPERhdGFUeXBlIHhtbG5zPSJodHRwOi8vd3d3LnNpbGEtc3RhbmRhcmQub3JnIj48TGlzdD48RGF0YVR5cGU+PEJhc2ljPkFueTwvQmFzaWM+PC9EYXRhVHlwZT48L0xpc3Q+PC9EYXRhVHlwZT4SXQpbCk88RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5TdHJpbmc8L0Jhc2ljPjwvRGF0YVR5cGU+EggKBgoEaWQtMQpjCgcKBWNvdW50ElgKUDxEYXRhVHlwZSB4bWxucz0iaHR0cDovL3d3dy5zaWxhLXN0YW5kYXJkLm9yZyI+PEJhc2ljPkludGVnZXI8L0Jhc2ljPjwvRGF0YVR5cGU+EgQKAggBCmUKCQoHc3VjY2VzcxJYClA8RGF0YVR5cGUgeG1sbnM9Imh0dHA6Ly93d3cuc2lsYS1zdGFuZGFyZC5vcmciPjxCYXNpYz5Cb29sZWFuPC9CYXNpYz48L0RhdGFUeXBlPhIECgIIAQ==",
                json!({"Body": {"data": {"addedIds": ["id-1"], "count": 1}, "success": true}}),
            ),
        ];
        for (type_xml_without_namespace, payload, expected) in cases {
            let type_xml = type_xml_without_namespace.replacen(
                "<DataType>",
                r#"<DataType xmlns="http://www.sila-standard.org">"#,
                1,
            );
            let payload = base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap();
            assert_eq!(decode(&type_xml, &payload), Ok(expected), "{type_xml}");
        }
        let void_payload = base64::engine::general_purpose::STANDARD
            .decode("CgA=")
            .unwrap();
        assert_eq!(decode(VOID_XML, &void_payload), Ok(Value::Null));
    }

    #[test]
    fn test_decode_reports_why_a_value_cannot_be_read() {
        let nan_payload: [u8; 11] = [0x0A, 0x09, 0x09, 0, 0, 0, 0, 0, 0, 0xF8, 0x7F];
        let bad_tenth_varint_byte: [u8; 11] = [
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ];
        let cases: [(&str, &[u8], DecodeError); 9] = [
            (
                STRING_XML,
                &[0x0A, 0x02, 0x08, 0x01],
                DecodeError::MalformedPayload,
            ),
            (
                INTEGER_XML,
                &[0x0A, 0x03, 0x0A, 0x01, 0x41],
                DecodeError::MalformedPayload,
            ),
            ("<NotADataType/>", &[], DecodeError::UnsupportedType),
            (
                "<DataType><Basic>String</DataType>",
                &[],
                DecodeError::UnsupportedType,
            ),
            (STRING_XML, &[0xFF], DecodeError::MalformedPayload),
            (STRING_XML, &[0x00, 0x00], DecodeError::MalformedPayload),
            (
                STRING_XML,
                &bad_tenth_varint_byte,
                DecodeError::MalformedPayload,
            ),
            (
                STRING_XML,
                &[0x0A, 0x03, 0x0A, 0x01, 0xFF],
                DecodeError::InvalidUtf8,
            ),
            (REAL_XML, &nan_payload, DecodeError::NonFiniteReal),
        ];
        for (type_xml, payload, expected) in cases {
            assert_eq!(decode(type_xml, payload), Err(expected), "{type_xml}");
        }
    }

    #[test]
    fn test_decode_reads_the_last_copy_past_unknown_fields() {
        // Holder field 1 wraps: field 2 = 5, field 1 = 1, field 1 = 42.
        let payload = [0x0A, 0x06, 0x10, 0x05, 0x08, 0x01, 0x08, 0x2A];
        assert_eq!(decode(INTEGER_XML, &payload), Ok(json!(42)));
    }

    #[test]
    fn test_decode_accepts_well_formed_xml() {
        let type_xml = r#"<?xml version="1.0"?><!-- note --><DataType><Structure><Element><Identifier>A&amp;B</Identifier><DataType><Basic>Integer</Basic></DataType></Element></Structure></DataType>"#;
        let payload = [0x0A, 0x04, 0x0A, 0x02, 0x08, 0x07];
        assert_eq!(decode(type_xml, &payload), Ok(json!({"A&B": 7})));
    }

    #[test]
    fn test_undecodable_any_values_are_left_alone() {
        let any_holder = load_descriptor_pool()
            .get_message_by_name("sila2.org.silastandard.AnyHolder")
            .unwrap();
        let junk = json!({ "type": "<NotADataType/>", "payload": "//8=" });
        let mut message = json!({ "payload": junk.clone() });
        unpack_any_fields(&any_holder, &mut message);
        assert_eq!(message["payload"], junk);
    }

    struct Echo;

    #[tonic::async_trait]
    impl AnyEcho for Echo {
        async fn echo(&self, request: Request<AnyHolder>) -> Result<Response<AnyHolder>, Status> {
            Ok(Response::new(request.into_inner()))
        }
    }

    /// Fails if either codec hook in grpc.rs is missing.
    #[test]
    fn test_grpc_node_round_trips_every_any_position() {
        let descriptor_set_bytes =
            include_bytes!(concat!(env!("OUT_DIR"), "/file_descriptor_set.bin"));
        DescriptorPool::decode_global_file_descriptor_set(&descriptor_set_bytes[..]).unwrap();

        let mut fixture = DiagramTestFixture::new();
        // The grpc.rs tests use port 50000 + line!(). Tests run in parallel, so
        // this file starts at 51000.
        let port = 51000 + line!();
        let address = format!("[::1]:{port}").parse().unwrap();

        let runtime = Arc::new(Runtime::new().unwrap());
        runtime.spawn(async move {
            Server::builder()
                .add_service(AnyEchoServer::new(Echo))
                .serve(address)
                .await
                .unwrap();
        });
        fixture.registry.enable_grpc(Arc::clone(&runtime), None);

        let (exit_sender, exit_receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = runtime.block_on(exit_receiver);
        });

        let diagram = Diagram::from_json(json!({
            "version": "0.1.0",
            "start": "echo",
            "ops": {
                "echo": {
                    "type": "node",
                    "builder": "grpc_request",
                    "config": {
                        "service": "sila2.org.silastandard.AnyEcho",
                        "method": "Echo",
                        "uri": format!("http://localhost:{port}"),
                    },
                    "stream_out": {
                        "out": { "builtin": "terminate" }
                    },
                    "next": { "builtin": "terminate" }
                }
            }
        }))
        .unwrap();

        let inputs: Vec<Value> = make_encode_cases().into_iter().map(|case| case.0).collect();
        let request = json!({
            "name": { "value": "keep me" },
            "plain": "keep me too",
            "payload": { "Mode": "active", "Body": { "n": 1 } },
            "items": inputs,
            "named": { "first": true, "second": null },
            "child": { "payload": [1, 2], "child": { "payload": "deep" } }
        });

        let response: JsonMessage = fixture
            .spawn_and_run_with_conditions(&diagram, request.clone(), Duration::from_secs(2))
            .unwrap();

        for field in ["name", "plain", "payload", "items", "named"] {
            assert_eq!(response[field], request[field], "round trip of {field}");
        }
        assert_eq!(response["child"]["payload"], json!([1, 2]));
        assert_eq!(response["child"]["child"]["payload"], "deep");

        let _ = exit_sender.send(());
    }
}
