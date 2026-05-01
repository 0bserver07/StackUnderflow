"""Print every coding-agent project StackUnderflow can see on this machine.

Run with: python examples/list_projects.py
"""

import stackunderflow

for p in stackunderflow.list_projects():
    # p is a dict with keys: dir_name, log_path, file_count, total_size_mb,
    # last_modified, first_seen, display_name
    print(f"{p['dir_name']}  ({p['file_count']} files, {p['total_size_mb']} MB)")
