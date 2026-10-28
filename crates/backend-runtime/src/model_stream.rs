//! Incremental OpenAI SSE framing; incomplete streams never become committed turns.
use crate::{Fault, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    data: Vec<String>,
    content: String,
    calls: BTreeMap<u64, Call>,
    done: bool,
    finished: bool,
}
impl Decoder {
    pub fn is_done(&self) -> bool {
        self.done
    }
    pub fn feed(
        &mut self,
        chunk: &[u8],
        emit: &mut (dyn FnMut(Value) -> Result<()> + Send),
    ) -> Result<()> {
        if self.pending.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=end).collect();
            let line = std::str::from_utf8(&raw)
                .map_err(|_| Fault::Protocol)?
                .trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                self.event(emit)?;
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
            if self.data.iter().map(String::len).sum::<usize>() > 1024 * 1024 {
                return Err(Fault::Capacity);
            }
        }
        Ok(())
    }
    fn event(&mut self, emit: &mut (dyn FnMut(Value) -> Result<()> + Send)) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data).join("\n");
        if self.done {
            return Err(Fault::Protocol);
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let row: Value = serde_json::from_str(&data).map_err(|_| Fault::Protocol)?;
        if row.get("error").is_some() {
            return Err(Fault::Protocol);
        }
        let choices = row
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(Fault::Protocol)?;
        if choices.is_empty() {
            return Ok(());
        } // terminal usage frame
        if choices.len() != 1 {
            return Err(Fault::Protocol);
        }
        let choice = choices.first().ok_or(Fault::Protocol)?;
        if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
            return Err(Fault::Protocol);
        }
        let delta = choice
            .get("delta")
            .and_then(Value::as_object)
            .ok_or(Fault::Protocol)?;
        if delta.get("role").is_some_and(|r| r != "assistant") {
            return Err(Fault::Protocol);
        }
        for (field, event) in [
            ("reasoning_content", "reasoning"),
            ("reasoning", "reasoning"),
            ("content", "delta"),
        ] {
            if let Some(value) = delta.get(field).filter(|v| !v.is_null()) {
                let text = value.as_str().ok_or(Fault::Protocol)?;
                if self.finished && !text.is_empty() {
                    return Err(Fault::Protocol);
                }
                if field == "content" {
                    if self.content.len().saturating_add(text.len()) > 4 * 1024 * 1024 {
                        return Err(Fault::Capacity);
                    }
                    self.content.push_str(text);
                }
                if !text.is_empty() {
                    emit(json!({"type":event,"text":text}))?;
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            if self.finished {
                return Err(Fault::Protocol);
            }
            for row in calls.as_array().ok_or(Fault::Protocol)? {
                let index = row
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or(Fault::Protocol)?;
                if index >= 32 {
                    return Err(Fault::Capacity);
                }
                let call = self.calls.entry(index).or_default();
                if let Some(id) = row.get("id").filter(|v| !v.is_null()) {
                    append(&mut call.id, id, 256)?;
                }
                if row.get("type").is_some_and(|v| v != "function") {
                    return Err(Fault::Protocol);
                }
                if let Some(function) = row.get("function") {
                    if let Some(name) = function.get("name").filter(|v| !v.is_null()) {
                        append(&mut call.name, name, 128)?;
                    }
                    if let Some(arguments) = function.get("arguments").filter(|v| !v.is_null()) {
                        append(&mut call.arguments, arguments, 1024 * 1024)?;
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
            if !matches!(reason.as_str(), Some("stop" | "tool_calls")) {
                return Err(Fault::Protocol);
            }
            self.finished = true;
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Value> {
        if !self.done
            || !self.finished
            || !self.pending.iter().all(u8::is_ascii_whitespace)
            || !self.data.is_empty()
        {
            return Err(Fault::Protocol);
        }
        let mut result = json!({"role":"assistant","content":self.content});
        if !self.calls.is_empty() {
            let mut calls = Vec::new();
            for (index, call) in self.calls {
                if index != calls.len() as u64 || call.id.is_empty() || call.name.is_empty() {
                    return Err(Fault::Protocol);
                }
                calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}));
            }
            result
                .as_object_mut()
                .ok_or(Fault::Protocol)?
                .insert("tool_calls".into(), json!(calls));
        }
        Ok(result)
    }
}
fn append(target: &mut String, value: &Value, limit: usize) -> Result<()> {
    let value = value.as_str().ok_or(Fault::Protocol)?;
    if target.len().saturating_add(value.len()) > limit {
        return Err(Fault::Capacity);
    }
    target.push_str(value);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_utf8_content_and_calls_stream_once_and_require_completion() -> Result<()> {
        let wire=concat!("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hé\"},\"finish_reason\":null}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call1\",\"type\":\"function\",\"function\":{\"name\":\"tool\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n");
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for byte in wire.as_bytes() {
            decoder.feed(&[*byte], &mut |v| {
                events.push(v);
                Ok(())
            })?;
        }
        let result = decoder.finish()?;
        assert_eq!(events.len(), 1);
        assert_eq!(result.get("content"), Some(&json!("hé")));
        assert_eq!(
            result.pointer("/tool_calls/0/function/arguments"),
            Some(&json!("{}"))
        );
        assert_eq!(Decoder::default().finish(), Err(Fault::Protocol));
        Ok(())
    }
}
