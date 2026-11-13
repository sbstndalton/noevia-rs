//! Checked public helpers. Internal row processing uses its separately validated
//! row domain rather than applying query-size checks to transformed row text.
use crate::{
    Entity, Error, Kind, NumericValue, MAX_EARLIER, MAX_EARLIER_BYTES, MAX_ENTITIES,
    MAX_QUERY_BYTES, MAX_WORDS, MAX_WORD_CHARS,
};

pub(crate) fn text(value: &str) -> Result<(), Error> {
    if value.len() > MAX_QUERY_BYTES {
        return Err(Error::Capacity);
    }
    Ok(())
}
pub(crate) fn earlier(messages: &[String]) -> Result<(), Error> {
    if messages.len() > MAX_EARLIER {
        return Err(Error::Capacity);
    }
    let mut total = 0usize;
    for message in messages {
        text(message)?;
        total = total.saturating_add(message.len());
        if total > MAX_EARLIER_BYTES {
            return Err(Error::Capacity);
        }
    }
    Ok(())
}
pub(crate) fn entities(values: &[Entity]) -> Result<(), Error> {
    if values.len() > MAX_ENTITIES {
        return Err(Error::Capacity);
    }
    for entity in values {
        text(&entity.name)?;
        if entity.keys.len() > MAX_WORDS {
            return Err(Error::Capacity);
        }
        let mut bytes = 0usize;
        for key in &entity.keys {
            text(key)?;
            bytes = bytes.saturating_add(key.len());
            if bytes > MAX_QUERY_BYTES {
                return Err(Error::Capacity);
            }
        }
        // Empty key sequences intentionally classify as absent, as in Python.
        // Individual keys and a named entity must be meaningful strings.
        if entity.keys.iter().any(String::is_empty)
            || (!entity.keys.is_empty() && entity.name.is_empty())
        {
            return Err(Error::InvalidEntity);
        }
    }
    Ok(())
}
pub(crate) fn numeric(value: &NumericValue) -> Result<(), Error> {
    if matches!(value, NumericValue::Text(s) if s.len() > 128) {
        return Err(Error::Capacity);
    }
    Ok(())
}
pub fn asked_entities(message: &str) -> Result<Vec<Entity>, Error> {
    text(message)?;
    crate::lexical::asked_entities(message)
}
pub fn carried_entities(message: &str, previous: &[String]) -> Result<Vec<Entity>, Error> {
    text(message)?;
    earlier(previous)?;
    crate::lexical::carried_entities(message, previous)
}
pub fn classify(value: &str, entity: &Entity) -> Result<(Option<Kind>, Option<String>), Error> {
    text(value)?;
    entities(std::slice::from_ref(entity))?;
    crate::lexical::classify(value, entity)
}
pub fn identified_people(value: &str, asked: Option<&[Entity]>) -> Result<Vec<Vec<String>>, Error> {
    text(value)?;
    if let Some(asked) = asked {
        entities(asked)?;
    }
    crate::people::identified_people(value, asked)
}
pub fn refers_back(message: &str) -> Result<bool, Error> {
    text(message)?;
    crate::lexical::refers_back(message)
}
pub fn relational(message: &str, values: &[Entity]) -> Result<bool, Error> {
    text(message)?;
    entities(values)?;
    crate::lexical::relational(message, values)
}
pub fn near_word(a: &str, b: &str) -> Result<bool, Error> {
    for value in [a, b] {
        text(value)?;
        if value.chars().count() > MAX_WORD_CHARS
            || crate::lexical::key(value).chars().count() > MAX_WORD_CHARS
        {
            return Err(Error::Capacity);
        }
    }
    crate::lexical::near_word(a, b)
}
/// Python `entity_focus._keys(words)`: comparison keys of name words (titles dropped, hyphenated words
/// split). Used by the Diary follow-up resolver to check that a name phrase is exactly one entity. The
/// words come from a whitespace split of text that already passed the query cap, so only the aggregate
/// byte total is checked here.
pub fn name_keys(words: &[String]) -> Result<Vec<String>, Error> {
    let mut total = 0usize;
    for word in words {
        total = total.saturating_add(word.len());
        if total > MAX_QUERY_BYTES {
            return Err(Error::Capacity);
        }
    }
    Ok(crate::lexical::keys(words))
}
pub fn parse_margin(value: &NumericValue) -> Result<f64, Error> {
    numeric(value)?;
    Ok(crate::policy::parse_margin(value))
}
