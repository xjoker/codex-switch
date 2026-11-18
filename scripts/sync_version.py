#!/usr/bin/env python3
import sys
import os

# Paths
base_dir = "/Users/xjoker/Developer/Repos/codex-switch"
version_file = os.path.join(base_dir, "VERSION")
cargo_file = os.path.join(base_dir, "Cargo.toml")

def get_version():
    with open(version_file, "r") as f:
        return f.read().strip()

def update_cargo(version):
    with open(cargo_file, "r") as f:
        lines = f.readlines()
    
    new_lines = []
    updated = False
    for line in lines:
        if line.startswith('version = "'):
            # Only update the package version line, not dependencies
            if ' = ' in line and not line.startswith(' '):
                new_lines.append(f'version = "{version}"\n')
                updated = True
                continue
        new_lines.append(line)
        
    if updated:
        with open(cargo_file, "w") as f:
            f.writelines(new_lines)
        print(f"Updated Cargo.toml to version {version}")
    else:
        print("Could not find version line in Cargo.toml")

if __name__ == "__main__":
    version = get_version()
    update_cargo(version)
