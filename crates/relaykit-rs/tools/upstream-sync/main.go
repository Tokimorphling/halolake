// Development-only oracle. Run in the pinned upstream relaykit module, never at crate runtime.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"github.com/QuantumNous/new-api/relaykit/dto"
	rc "github.com/QuantumNous/new-api/relaykit/relayconvert"
	"github.com/QuantumNous/new-api/relaykit/relayconvert/convmeta"
	"github.com/QuantumNous/new-api/relaykit/relayconvert/reasoning"
	"github.com/QuantumNous/new-api/relaykit/types"
	"os"
)

type Case struct {
	Name               string                        `json:"name"`
	Kind               string                        `json:"kind"`
	From               types.RelayFormat             `json:"from"`
	To                 types.RelayFormat             `json:"to"`
	Input              json.RawMessage               `json:"input"`
	Chunks             []json.RawMessage             `json:"chunks"`
	DefaultMax         int                           `json:"default_max"`
	Model              string                        `json:"model"`
	Policy             types.ConversionLossPolicy    `json:"policy"`
	Golden             bool                          `json:"golden"`
	OriginModel        string                        `json:"origin_model"`
	OpenRouter         bool                          `json:"open_router"`
	ClaudeAdapter      bool                          `json:"claude_adapter"`
	GeminiAdapter      bool                          `json:"gemini_adapter"`
	AdapterPercentage  float64                       `json:"adapter_percentage"`
	Reasoning          *dto.ReasoningConversionState `json:"reasoning_conversion"`
	SupportsJSONSchema bool                          `json:"supports_json_schema"`
	IncludeUsage       bool                          `json:"include_usage"`
	EmitSequenceNumber bool                          `json:"emit_sequence_number"`
}

func decode(k string, f types.RelayFormat, b json.RawMessage) (any, error) {
	var v any
	switch k {
	case "request":
		switch f {
		case "openai":
			v = &dto.GeneralOpenAIRequest{}
		case "openai_responses":
			v = &dto.OpenAIResponsesRequest{}
		case "claude":
			v = &dto.ClaudeRequest{}
		case "gemini":
			v = &dto.GeminiChatRequest{}
		}
	default:
		switch f {
		case "openai":
			if k == "stream" {
				v = &dto.ChatCompletionsStreamResponse{}
			} else {
				v = &dto.OpenAITextResponse{}
			}
		case "openai_responses":
			if k == "stream" {
				v = &dto.ResponsesStreamResponse{}
			} else {
				v = &dto.OpenAIResponsesResponse{}
			}
		case "claude":
			v = &dto.ClaudeResponse{}
		case "gemini":
			v = &dto.GeminiChatResponse{}
		}
	}
	if v == nil {
		return nil, fmt.Errorf("unsupported format")
	}
	return v, json.Unmarshal(b, v)
}
func main() {
	rc.SetMediaResolver(rc.MediaResolver{GetBase64Data: func(context.Context, types.FileSource, ...string) (string, string, error) {
		return "aGVsbG8=", "image/png", nil
	}, DecodeBase64FileData: func(string) (string, string, error) { return "aGVsbG8=", "image/png", nil }})
	scan := bufio.NewScanner(os.Stdin)
	scan.Buffer(make([]byte, 4096), 16*1024*1024)
	enc := json.NewEncoder(os.Stdout)
	for scan.Scan() {
		var c Case
		if e := json.Unmarshal(scan.Bytes(), &c); e != nil {
			panic(e)
		}
		result := map[string]any{"name": c.Name}
		if c.Kind == "deepseek_suffix" {
			var input struct {
				Model string `json:"model"`
			}
			if err := json.Unmarshal(c.Input, &input); err != nil {
				panic(err)
			}
			base, thinkingType, effort, ok := reasoning.ParseDeepSeekV4ThinkingSuffix(input.Model)
			result["value"] = map[string]any{"base_model": base, "thinking_type": thinkingType, "effort": effort, "ok": ok}
			if err := enc.Encode(result); err != nil {
				panic(err)
			}
			continue
		}
		if c.Kind == "dto" {
			var value any
			switch c.Model {
			case "GeneralOpenAIRequest":
				value = &dto.GeneralOpenAIRequest{}
			case "InputTokenDetails":
				value = &dto.InputTokenDetails{}
			case "FunctionRequest":
				value = &dto.FunctionRequest{}
			case "Tool":
				value = &dto.Tool{}
			case "ClaudeRequest":
				value = &dto.ClaudeRequest{}
			case "Thinking":
				value = &dto.Thinking{}
			case "ResponsesOutput":
				value = &dto.ResponsesOutput{}
			case "ChannelOtherSettings":
				value = &dto.ChannelOtherSettings{}
			case "UserSetting":
				value = &dto.UserSetting{}
			case "DeepSeekToolsRequest":
				value = &dto.DeepSeekToolsRequest{}
			default:
				panic("unsupported DTO: " + c.Model)
			}
			if err := json.Unmarshal(c.Input, value); err != nil {
				result["error"] = err.Error()
			} else {
				result["value"] = value
				serialized, err := json.Marshal(value)
				if err != nil {
					panic(err)
				}
				result["serialized_value"] = string(serialized)
			}
			if err := enc.Encode(result); err != nil {
				panic(err)
			}
			continue
		}
		opts := &convmeta.Options{ToolLossPolicy: c.Policy}
		if c.DefaultMax > 0 {
			opts.Claude.DefaultMaxTokens = func(string) int { return c.DefaultMax }
		}
		if c.Golden {
			opts.Gemini.FunctionCallThoughtSignatureEnabled = true
			opts.Gemini.ThinkingAdapterBudgetTokensPercentage = .6
			opts.Gemini.SafetySetting = func(string) string { return "OFF" }
		}
		opts.OpenRouterDialect = c.OpenRouter
		if c.SupportsJSONSchema {
			opts.Gemini.SupportsJSONSchema = func(string) bool { return true }
		}
		opts.Claude.ThinkingAdapterEnabled = c.ClaudeAdapter
		opts.Gemini.ThinkingAdapterEnabled = c.GeminiAdapter
		if c.AdapterPercentage != 0 {
			opts.Claude.ThinkingAdapterBudgetTokensPercentage = c.AdapterPercentage
			opts.Gemini.ThinkingAdapterBudgetTokensPercentage = c.AdapterPercentage
		}
		meta := &convmeta.Values{Options: opts, UpstreamModelName: c.Model, OriginModelName: c.OriginModel, ReasoningConversion: c.Reasoning, ChannelMetaAttached: c.Model != ""}
		var e error
		if c.Kind == "stream" {
			var state *rc.ResponseStreamState
			state, e = rc.NewResponseStreamState(c.From, c.To, rc.ResponseStreamOptions{
				ID: "stream_fixed", Model: "stream-model",
				IncludeUsage: c.IncludeUsage, EmitSequenceNumber: c.EmitSequenceNumber,
			})
			events := []any{}
			if e == nil {
				for _, b := range c.Chunks {
					var v any
					v, e = decode(c.Kind, c.From, b)
					if e != nil {
						break
					}
					var rs []rc.ResponseResult
					rs, e = rc.ConvertStreamResponseChunk(nil, meta, state, v)
					if e != nil {
						break
					}
					for _, r := range rs {
						events = append(events, r.Value)
					}
				}
				if e == nil {
					var rs []rc.ResponseResult
					rs, e = rc.FinalizeStreamResponse(nil, meta, state)
					for _, r := range rs {
						events = append(events, r.Value)
					}
				}
				result["usage"] = state.Usage()
				result["diagnostics"] = state.Diagnostics()
			}
			result["value"] = events
		} else {
			var v any
			v, e = decode(c.Kind, c.From, c.Input)
			if e == nil {
				if c.Kind == "request" {
					var r *rc.RequestResult
					r, e = rc.ConvertRequest(nil, meta, c.To, v)
					if r != nil {
						result["value"] = r.Value
						result["diagnostics"] = r.Diagnostics
						result["converter"] = r.Converter
						result["quality"] = r.Quality
					}
				} else {
					var r *rc.ResponseResult
					r, e = rc.ConvertResponse(nil, meta, c.To, v)
					if r != nil {
						result["value"] = r.Value
						result["usage"] = r.Usage
						result["diagnostics"] = r.Diagnostics
						result["converter"] = r.Converter
						result["quality"] = r.Quality
					}
				}
			}
		}
		if e != nil {
			result["error"] = e.Error()
		} else if value, ok := result["value"]; ok {
			serialized, err := json.Marshal(value)
			if err != nil {
				panic(err)
			}
			result["serialized_value"] = string(serialized)
		}
		if e = enc.Encode(result); e != nil {
			panic(e)
		}
	}
	if e := scan.Err(); e != nil {
		panic(e)
	}
}
