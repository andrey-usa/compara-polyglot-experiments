import os

here = "exec_helper.py here" if os.path.exists("exec_helper.py") else "elsewhere"
base = os.environ.get("PRIMAVERA_BASE_URL", "none").split(":")[0]
print(f"{here}; base {base}")
