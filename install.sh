#!/bin/bash
set -e

INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Installing sgpu..."
pip install -e "$INSTALL_DIR"

echo ""
echo "Done! Commands available:"
echo "  sgpu                       # Launch TUI"
echo "  sgpu-collector --daemon    # Start background collector"
echo "  sgpu-collector --stop      # Stop collector"
echo "  sgpu-collector --status    # Check collector status"
