#!/bin/bash

if [ -n "$1" ]; then
    COMMIT_MSG="$1"
else
    # Try to get the commit message from the clipboard
    if command -v pbpaste > /dev/null 2>&1; then
        COMMIT_MSG=$(pbpaste)
    elif command -v xclip > /dev/null 2>&1; then
        COMMIT_MSG=$(xclip -selection clipboard -o)
    else
        echo "Neither a commit message nor clipboard content found."
        exit 1
    fi
fi

if [[ ! $COMMIT_MSG =~ ^(https?|ftp):// ]]; then
    echo "Commit message does not appear to be a URI."
    exit 1
fi

git commit --allow-empty -m "$COMMIT_MSG" -S