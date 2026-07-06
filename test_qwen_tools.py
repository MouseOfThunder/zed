#!/usr/bin/env python3
"""Test Qwen 3.6 tool calling with edit_file schema."""
import json, requests

API = "http://localhost:8000/v1/chat/completions"

# Das volle edit_file Schema wie Zed es sendet (unser stripped format)
edit_file_tool = {
    "type": "function",
    "function": {
        "name": "edit_file",
        "description": "Edit a file by replacing lines",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The full path of the file to edit in the project."
                },
                "start_line": {
                    "type": ["integer", "null"]
                },
                "end_line": {
                    "type": ["integer", "null"]
                },
                "new_text": {
                    "type": "string",
                    "description": "New text that replaces lines start_line..=end_line."
                },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {"type": "string"},
                            "new_text": {"type": "string"},
                            "start_line": {"type": ["integer", "null"]},
                            "end_line": {"type": ["integer", "null"]}
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        }
    }
}

read_file_tool = {
    "type": "function",
    "function": {
        "name": "read_file",
        "description": "Read a file",
        "parameters": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start_line": {"type": ["integer", "null"]},
                "end_line": {"type": ["integer", "null"]}
            },
            "required": ["path"]
        }
    }
}

def test(prompt, tools, label):
    print(f"\n{'='*60}")
    print(f"TEST: {label}")
    print(f"Prompt: {prompt}")
    print(f"Tools: {[t['function']['name'] for t in tools]}")
    print(f"{'='*60}")

    payload = {
        "model": "qwen3.6-35b-ud-nvfp4-xl",
        "messages": [
            {"role": "system", "content": "You are a coding assistant. Use tools to help the user."},
            {"role": "user", "content": prompt}
        ],
        "tools": tools,
        "temperature": 0.3,
        "max_tokens": 2000,
        "stream": False
    }

    r = requests.post(API, json=payload, timeout=120)
    print(f"Status: {r.status_code}")
    msg = r.json()["choices"][0]["message"]

    if msg.get("tool_calls"):
        for tc in msg["tool_calls"]:
            fn = tc["function"]
            print(f"  Tool call: {fn['name']}")
            try:
                args = json.loads(fn["arguments"])
                print(f"  Arguments: {json.dumps(args, indent=2)}")
            except json.JSONDecodeError as e:
                print(f"  INVALID JSON: {fn['arguments'][:200]}")
                print(f"  Error: {e}")
    else:
        print(f"  Text response: {msg.get('content', '')[:300]}")

    print(f"  Finish: {r.json()['choices'][0].get('finish_reason')}")

# Test 1: Simple read_file (sollte klappen)
test("Read the file src/main.rs", [read_file_tool], "flaches Schema: read_file")

# Test 2: edit_file original (nested edits)
test("Edit src/main.rs to add a comment at line 1", [read_file_tool, edit_file_tool], "Nested Schema: edit_file (edits array)")

# Test 3: minimal edit - nur eine Zeile
test("Change line 5 of src/config.py from 'debug = False' to 'debug = True'",
     [read_file_tool, edit_file_tool], "Einfacher edit_file call")

print("\nDONE")
