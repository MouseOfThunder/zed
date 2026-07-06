import re

path = "/Users/arndhekermans/Projekte/zed-fork/crates/agent/src/tools/replace_lines_tool.rs"
with open(path) as f:
    content = f.read()
content = content.replace("Rc::new(ReplaceLinesTool", "Arc::new(ReplaceLinesTool")
with open(path, "w") as f:
    f.write(content)
print("done")
