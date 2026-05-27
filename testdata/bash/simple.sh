#!/bin/bash

source ./utils.sh

# A greeting function
greet() {
    local name="$1"
    echo "Hello, ${name}!"
}

# A formatting function
format_name() {
    echo "$1" | tr '[:lower:]' '[:upper:]'
}

# Main logic
main() {
    local formatted
    formatted=$(format_name "world")
    greet "$formatted"
}

main "$@"
