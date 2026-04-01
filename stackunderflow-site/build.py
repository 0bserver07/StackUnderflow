#!/usr/bin/env python3
"""
Build script to bundle assets from stackunderflow package into static site
This runs during Cloudflare Pages build to create self-contained pages
"""

import sys
from pathlib import Path

# Add parent directory to path to import from stackunderflow
sys.path.insert(0, str(Path(__file__).parent.parent))


def build_share_template():
    """Build share.html with all assets bundled"""

    # Read CSS from stackunderflow package
    css_path = Path(__file__).parent.parent / "stackunderflow" / "static" / "css" / "dashboard.css"
    with open(css_path) as f:
        dashboard_css = f.read()

    # Read JavaScript files from stackunderflow
    # Note: commands-tab.js is excluded as share-viewer.js has its own implementation
    js_files = ["constants.js", "utils.js", "stats.js", "stats-cards.js", "message-modal.js", "charts.js"]
    combined_js = []

    for js_file in js_files:
        js_path = Path(__file__).parent.parent / "stackunderflow" / "static" / "js" / js_file
        with open(js_path) as f:
            combined_js.append(f"// === {js_file} ===\n{f.read()}\n")

    # Add share-viewer.js from stackunderflow-site
    share_viewer_path = Path(__file__).parent / "static" / "js" / "share-viewer.js"
    with open(share_viewer_path) as f:
        combined_js.append(f"// === share-viewer.js ===\n{f.read()}\n")

    # Read share.html template
    template_path = Path(__file__).parent / "share-template.html"
    with open(template_path) as f:
        template = f.read()

    # Replace placeholders
    template = template.replace("/* DASHBOARD_STYLES */", dashboard_css)
    template = template.replace("// DASHBOARD_SCRIPTS", "\n".join(combined_js))

    # Write final share.html
    output_path = Path(__file__).parent / "share.html"
    with open(output_path, "w") as f:
        f.write(template)

    print("Built share.html with bundled assets")


def copy_static_assets():
    """Copy any other needed static assets"""
    # Copy images, fonts, etc. if needed
    pass


if __name__ == "__main__":
    build_share_template()
    copy_static_assets()
