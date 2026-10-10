//! 极简 JSON 解析器。
//!
//! 仅用于解析本程序自己发起的 PowerShell `ConvertTo-Json` 输出。
//! 支持 object / array / string / number / bool / null，不依赖任何第三方 crate。

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        // PowerShell 管道输出可能带 UTF-8 BOM
        let cleaned = text.trim_start_matches('\u{feff}');
        let mut p = Parser {
            b: cleaned.as_bytes(),
            i: 0,
        };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.i != p.b.len() {
            return Err(format!("JSON 结尾存在多余内容（位置 {}）", p.i));
        }
        Ok(v)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// 若该值不是数组则包装成长度为 1 的数组（PowerShell 单元素时会退化成对象）。
    pub fn as_arr_or_single(&self) -> Vec<&Json> {
        match self {
            Json::Arr(a) => a.iter().collect(),
            Json::Null => Vec::new(),
            other => vec![other],
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\t' | b'\r' | b'\n' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("位置 {}：无法识别的 JSON 值", self.i)),
        }
    }

    fn literal(&mut self, lit: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(format!("位置 {}：期望 {} 字面量", self.i, lit))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.i += 1; // {
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("位置 {}：对象缺少 ':'", self.i));
            }
            self.i += 1;
            self.skip_ws();
            let val = self.value()?;
            out.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(format!("位置 {}：对象缺少 ',' 或 '}}'", self.i)),
            }
        }
        Ok(Json::Obj(out))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.i += 1; // [
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            self.skip_ws();
            out.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(format!("位置 {}：数组缺少 ',' 或 ']'", self.i)),
            }
        }
        Ok(Json::Arr(out))
    }

    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("位置 {}：期望字符串", self.i));
        }
        self.i += 1;
        let mut s = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(s);
                }
                b'\\' => {
                    self.i += 1;
                    let Some(e) = self.peek() else {
                        return Err("字符串中的转义不完整".into());
                    };
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            if self.i + 4 > self.b.len() {
                                return Err("\\u 转义不完整".into());
                            }
                            let hex = std::str::from_utf8(&self.b[self.i..self.i + 4])
                                .map_err(|_| "\\u 转义非法".to_string())?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| "\\u 转义非法".to_string())?;
                            self.i += 4;
                            s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => s.push(other as char),
                    }
                }
                _ => {
                    // UTF-8 多字节字符按原样拷贝
                    let start = self.i;
                    let len = utf8_len(c);
                    self.i = (self.i + len).min(self.b.len());
                    s.push_str(&String::from_utf8_lossy(&self.b[start..self.i]));
                }
            }
        }
        Err("字符串未闭合".into())
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                self.i += 1;
            } else {
                break;
            }
        }
        let text =
            std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "数字解析失败".to_string())?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("非法数字：{}", text))
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_object() {
        let v = Json::parse(r#"{"MemoryCompression":true,"PageCombining":false}"#).unwrap();
        assert_eq!(
            v.get("MemoryCompression").and_then(Json::as_bool),
            Some(true)
        );
        assert_eq!(v.get("PageCombining").and_then(Json::as_bool), Some(false));
    }

    #[test]
    fn parses_nested_and_arrays() {
        let src = r#"{"Automatic":true,"Settings":[{"Name":"C:\\pagefile.sys","InitialSize":0}],"Usage":[]}"#;
        let v = Json::parse(src).unwrap();
        assert_eq!(v.get("Automatic").and_then(Json::as_bool), Some(true));
        let settings = v.get("Settings").unwrap().as_arr().unwrap();
        assert_eq!(settings.len(), 1);
        assert_eq!(
            settings[0].get("Name").and_then(Json::as_str),
            Some(r"C:\pagefile.sys")
        );
        assert!(v.get("Usage").unwrap().as_arr().unwrap().is_empty());
    }

    #[test]
    fn handles_null_and_bom() {
        assert!(Json::parse("\u{feff}null").unwrap().is_null());
        assert!(Json::parse(" null ").unwrap().is_null());
    }

    #[test]
    fn single_value_is_wrapped() {
        let v = Json::parse(r#"{"Name":"p.sys","CurrentUsage":12}"#).unwrap();
        assert_eq!(v.as_arr_or_single().len(), 1);
    }

    #[test]
    fn rejects_malformed() {
        assert!(Json::parse("{\"a\":}").is_err());
        assert!(Json::parse("[1,2").is_err());
        assert!(Json::parse("{} trailing").is_err());
    }

    #[test]
    fn parses_negative_and_float() {
        let v = Json::parse(r#"{"a":-12.5,"b":0}"#).unwrap();
        assert_eq!(v.get("a").and_then(Json::as_i64), Some(-12));
        assert_eq!(v.get("b").and_then(Json::as_f64), Some(0.0));
    }
}
