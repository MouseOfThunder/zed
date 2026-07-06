import json
import re

with open("/Users/arndhekermans/.config/zed/settings.json") as f:
    content = f.read()

# Remove // comments ONLY outside of strings
result = []
in_string = False
i = 0
while i < len(content):
    if not in_string and content[i : i + 2] == "//":
        # Comment until end of line
        while i < len(content) and content[i] != "\n":
            i += 1
        result.append("\n")
        i += 1
        continue
    if content[i] == '"' and (i == 0 or content[i - 1] != "\\"):
        in_string = not in_string
    result.append(content[i])
    i += 1

content = "".join(result)

# Remove trailing commas before } or ]
content = re.sub(r",(\s*[}\]])", r"\1", content)

s = json.loads(content)

s["agent"]["auto_compact"] = {"enabled": False}

tools = s["agent"]["tool_permissions"]["tools"]
tools["edit_file"] = {
    "default": "allow",
    "always_deny": [
        {"pattern": r"\.env($|\.)"},
        {"pattern": r"secrets?/"},
        {"pattern": r"\.pem$"},
        {"pattern": r"\.key$"},
    ],
}
tools["write_file"] = {
    "default": "allow",
    "always_deny": [
        {"pattern": r"\.env($|\.)"},
        {"pattern": r"secrets?/"},
        {"pattern": r"\.pem$"},
        {"pattern": r"\.key$"},
    ],
}

with open("/Users/arndhekermans/.config/zed/settings.json", "w") as f:
    json.dump(s, f, indent=2)
    f.write("\n")
print("done")
