#!/usr/bin/env python3
"""Second CGI demo: relative paths work because the server sets the cwd."""
import datetime
import os

print("Content-Type: text/plain; charset=utf-8")
print()
print(f"server time: {datetime.datetime.now().isoformat()}")
print(f"cgi working directory: {os.getcwd()}")
