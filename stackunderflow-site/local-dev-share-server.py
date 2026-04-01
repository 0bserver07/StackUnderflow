#!/usr/bin/env python3
"""
Local development server for testing shared dashboards
Simulates Cloudflare Pages Functions behavior
"""

import json
import os
import urllib.parse
from http.server import HTTPServer, SimpleHTTPRequestHandler
from pathlib import Path


class ShareHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        # Parse the URL
        parsed_path = urllib.parse.urlparse(self.path)
        path_parts = parsed_path.path.strip("/").split("/")

        # Check if this is a share request
        if len(path_parts) == 2 and path_parts[0] == "share":
            share_id = path_parts[1]
            self.serve_share(share_id)
        elif parsed_path.path == "/gallery-index.json":
            self.serve_gallery_index()
        else:
            # Serve static files normally
            super().do_GET()

    def serve_share(self, share_id):
        """Serve a shared dashboard"""
        # Look for the share data in fake-r2
        fake_r2_path = Path(__file__).parent.parent / "fake-r2" / f"{share_id}.json"

        if not fake_r2_path.exists():
            self.send_error(404, f"Share not found: {share_id}")
            return

        # Load share data
        with open(fake_r2_path) as f:
            share_data = json.load(f)

        # Build the share.html if it doesn't exist
        share_html_path = Path(__file__).parent / "share.html"
        if not share_html_path.exists():
            print("Building share.html...")
            os.system("python3 build.py")

        # Read the share.html template
        with open(share_html_path) as f:
            html_content = f.read()

        # Inject the share data
        html_content = html_content.replace("// SHARE_DATA_INJECTION", f"window.SHARE_DATA = {json.dumps(share_data)};")

        # Send the response
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(html_content.encode())

    def serve_gallery_index(self):
        """Serve the gallery index JSON"""
        gallery_path = Path(__file__).parent.parent / "fake-r2" / "gallery-index.json"

        if not gallery_path.exists():
            # Return empty gallery if file doesn't exist
            gallery_data = {"projects": []}
        else:
            with open(gallery_path) as f:
                gallery_data = json.load(f)

        # Send the response
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(json.dumps(gallery_data).encode())


def main():
    os.chdir(Path(__file__).parent)

    # Build share.html first
    print("Building share.html...")
    os.system("python3 build.py")

    # Start server
    port = 4001
    server = HTTPServer(("localhost", port), ShareHandler)
    print(f"\n🚀 Share server running at http://localhost:{port}")
    print(f"📁 Serving shares from: {Path(__file__).parent.parent / 'fake-r2'}")
    print("\nYou can now visit your share links!")
    print("Example: http://localhost:4001/share/0fc2cc5c-6c4a-47\n")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\nShutting down server...")
        server.shutdown()


if __name__ == "__main__":
    main()
