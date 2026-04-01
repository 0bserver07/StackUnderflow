#!/usr/bin/env python3
"""
Development server for the stackunderflow.dev homepage
Serves static files and proxies gallery requests to fake-r2
"""

import json
import os
from http.server import HTTPServer, SimpleHTTPRequestHandler
from pathlib import Path


class SiteHandler(SimpleHTTPRequestHandler):
    def do_GET(self):
        # Handle gallery-index.json request
        if self.path == "/gallery-index.json":
            self.serve_gallery_index()
        else:
            # Serve static files normally
            super().do_GET()

    def serve_gallery_index(self):
        """Serve the gallery index from fake-r2"""
        gallery_path = Path(__file__).parent.parent / "fake-r2" / "gallery-index.json"

        if not gallery_path.exists():
            gallery_data = {"projects": []}
        else:
            with open(gallery_path) as f:
                gallery_data = json.load(f)

        # Send the response
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(gallery_data).encode())


def main():
    os.chdir(Path(__file__).parent)

    port = 8000
    server = HTTPServer(("localhost", port), SiteHandler)
    print(f"\n🚀 Site server running at http://localhost:{port}")
    print("📁 Serving homepage and gallery")
    print("\nYou can now visit the homepage!")
    print(f"Example: http://localhost:{port}\n")

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\nShutting down server...")
        server.shutdown()


if __name__ == "__main__":
    main()
