#!/usr/bin/env python3
"""Demo CGI script: greets, echoes the request info and the body."""
import os
import sys
import urllib.parse

body = sys.stdin.buffer.read()  # the server closes stdin: EOF ends the body

query = dict(urllib.parse.parse_qsl(os.environ.get("QUERY_STRING", "")))
name = query.get("name", "stranger")

print("Status: 200 OK")
print("Content-Type: text/html; charset=utf-8")
print()
print(f"<h1>Hello, {name}!</h1>")
print("<ul>")
print(f"<li>method: {os.environ.get('REQUEST_METHOD')}</li>")
print(f"<li>script (PATH_INFO): {os.environ.get('PATH_INFO')}</li>")
print(f"<li>query string: {os.environ.get('QUERY_STRING')}</li>")
print(f"<li>content length: {os.environ.get('CONTENT_LENGTH')}</li>")
print(f"<li>body bytes received: {len(body)}</li>")
print("</ul>")
if body:
    text = body.decode("utf-8", "replace")
    print(f"<p>body: <code>{text}</code></p>")
print('<p><a href="/">home</a></p>')
