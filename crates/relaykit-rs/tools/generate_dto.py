"""Generate typed Rust DTO fields from the pinned upstream Go AST schema."""
import json,re,pathlib
root=pathlib.Path(__file__).resolve().parent.parent
schema=json.loads((root/'src/dto_schema.json').read_text())
def snake(s):
 s=re.sub(r'([A-Z]+)([A-Z][a-z])',r'\1_\2',s);s=re.sub(r'([a-z0-9])([A-Z])',r'\1_\2',s).lower();return 'r#'+s if s in ['type','ref','match','loop','self','use','mod','fn','as','in','box'] else s
def ty(t):
 if t.startswith('*'): return 'Option<Box<'+ty(t[1:])+'>>' if t[1:] in ['BillingUsage','Usage'] else 'Option<'+ty(t[1:])+'> '
 if t.startswith('[]'):return 'Option<Vec<'+ty(t[2:])+ '>>'
 if t.startswith('map[string]'):return 'Option<std::collections::BTreeMap<String, '+ty(t[11:])+ '>>'
 if t=='json.RawMessage':return 'Option<Value>'
 if t in schema:return t
 return {'string':'String','bool':'bool','int':'i64','int64':'i64','int32':'i32','uint':'u64','uint64':'u64','uint32':'u32','float64':'f64','float32':'f32','MediaResolution':'String','FunctionCallingConfigMode':'String','AwsApiType':'String','PriceType':'String','IntValue':'IntValue','BoolValue':'BoolValue','StringValue':'StringValue'}.get(t,'Value')
serial_custom={'GeneralOpenAIRequest','OpenAIResponsesRequest','ResponsesOutput','ToolCallRequest','Thinking'}
deserial_custom={'GeminiChatRequest','GeminiThinkingConfig','GeminiInlineData','GeminiPart','GeminiChatGenerationConfig','Thinking'}
out=['//! Protocol DTOs generated from the pinned Go field definitions.\n//! `Option<Vec<_>>` preserves the distinction between nil and empty slices.\nuse serde::{Serialize,Deserialize};\nuse serde_json::Value;\npub use crate::dto_values::{IntValue,BoolValue,StringValue};\n']
for name,fields in sorted(schema.items()):
 if not name[0].isupper(): continue
 derives=['Debug','Clone','Default','PartialEq']
 if name not in serial_custom: derives.append('Serialize')
 if name not in deserial_custom: derives.append('Deserialize')
 derived_serde = name not in serial_custom or name not in deserial_custom
 out += ['#[derive('+', '.join(derives)+')]']
 if derived_serde:out += ['#[serde(default)]']
 out += [f'pub struct {name} {{']
 for f in fields:
  if 'embed' in f:
   e=f['embed'];out+=['    #[serde(flatten)]',f'    pub {snake(e)}: {ty(e)},'];continue
  t=ty(f['type']).strip();attrs=[f'rename = "{f["json"]}"']
  if f['omit']:
   skip= ('option_empty' if f['type'].startswith(('[]','map[')) else 'Option::is_none') if t.startswith('Option<') else ('String::is_empty' if t=='String' else 'Value::is_null' if t=='Value' else 'is_default' if t in ['bool','i64','i32','u32','u64','f32','f64'] else None)
   if skip:attrs.append(f'skip_serializing_if = "{skip}"')
  if f['type']=='json.RawMessage':attrs.append('deserialize_with = "nullable_raw_message"' if name=='FunctionRequest' and f['name']=='Parameters' else 'deserialize_with = "raw_message"')
  elif not t.startswith('Option<') and t!='Value':attrs.append('deserialize_with = "null_default"')
  if derived_serde:out += [f'    #[serde({", ".join(attrs)})]']
  out += [f'    pub {snake(f["name"])}: {t},']
 if name=='ToolCallRequest':out += ['    /// Host-only complete tool definition, emitted verbatim when present.','    #[serde(skip)]','    pub native: Option<Value>,']
 if name=='Thinking':out += ['    /// Unmodeled thinking controls, forwarded verbatim like the Go DTO.','    pub extra: std::collections::BTreeMap<String, Value>,']
 out+=['}','']
 if name in serial_custom:
  out += [f'impl Serialize for {name} {{', '    fn serialize<S:serde::Serializer>(&self, serializer:S)->Result<S::Ok,S::Error>{']
  if name=='ToolCallRequest':out+=['        if let Some(native)=&self.native{return native.serialize(serializer)}']
  out+=['        let mut object=serde_json::Map::new();']
  for f in fields:
   if 'embed' in f:continue
   field=snake(f['name']);key=f['json'];t=ty(f['type']).strip();cond='true'
   if f['omit']:
    cond=f'!option_empty(&self.{field})' if f['type'].startswith(('[]','map[')) else f'self.{field}.is_some()' if t.startswith('Option<') else f'!self.{field}.is_empty()' if t=='String' else f'!self.{field}.is_null()' if t=='Value' else f'!is_default(&self.{field})' if t in ['bool','i64','i32','u32','u64','f32','f64'] else 'true'
   borrow='' if f['type'].lstrip('*') in ['bool','int','int32','int64','uint','uint32','uint64','float32','float64','IntValue','BoolValue'] else '&'
   out+=[f'        if {cond} {{object.insert({json.dumps(key)}.into(),serde_json::to_value({borrow}self.{field}).map_err(serde::ser::Error::custom)?);}}']
  if name=='Thinking':out += ['        for (key,value) in &self.extra {object.entry(key.clone()).or_insert_with(||value.clone());}','        Value::Object(object).serialize(serializer)']
  else:out += [f'        crate::wire::normalize_named("{name}",&Value::Object(object)).map_err(serde::ser::Error::custom)?.serialize(serializer)']
  out += ['    }','}']
 if name in deserial_custom:
  out += [f"impl<'de> Deserialize<'de> for {name} {{", "    fn deserialize<D:serde::Deserializer<'de>>(deserializer:D)->Result<Self,D::Error>{",'        let value=Value::deserialize(deserializer)?;',f'        let mut object=crate::wire::normalize_named("{name}",&value).map_err(serde::de::Error::custom)?.as_object().cloned().unwrap_or_default();','        Ok(Self{']
  for f in fields:
   if 'embed' in f:continue
   field=snake(f['name']);key=f['json']
   if name=='Thinking' and f['type']=='json.RawMessage':out += [f'            {field}: object.remove({json.dumps(key)}),']
   else:out += [f'            {field}: object.remove({json.dumps(key)}).map(serde_json::from_value).transpose().map_err(serde::de::Error::custom)?.unwrap_or_default(),']
  if name=='Thinking':out += ['            extra: object.into_iter().collect(),']
  out += ['        })','    }','}']
out += ["fn raw_message<'de,D:serde::Deserializer<'de>>(d:D)->Result<Option<Value>,D::Error>{Value::deserialize(d).map(Some)}"]
out += ["fn nullable_raw_message<'de,D:serde::Deserializer<'de>>(d:D)->Result<Option<Value>,D::Error>{Option::<Value>::deserialize(d)}"]
out += ['fn is_default<T: Default + PartialEq>(v:&T)->bool{*v==T::default()}','trait Empty {fn empty(&self)->bool;}','impl<T> Empty for Vec<T>{fn empty(&self)->bool{self.is_empty()}}','impl<K,V> Empty for std::collections::BTreeMap<K,V>{fn empty(&self)->bool{self.is_empty()}}','fn option_empty<T:Empty>(v:&Option<T>)->bool{v.as_ref().is_none_or(Empty::empty)}',"fn null_default<'de,D,T>(d:D)->Result<T,D::Error> where D:serde::Deserializer<'de>,T:Deserialize<'de>+Default {Ok(Option::<T>::deserialize(d)?.unwrap_or_default())}"]
(root/'src/dto.rs').write_text('\n'.join(out)+'\n')
