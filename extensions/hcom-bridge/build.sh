#!/bin/sh
# Build the hcom-bridge VS Code extension for Antigravity
set -e

cd "$(dirname "$0")"

echo "Installing dependencies..."
npm install --silent

echo "Compiling TypeScript..."
npx tsc -p tsconfig.json

echo "Bundling extension..."
npx esbuild src/extension.ts --bundle --outfile=dist/bundle.js \
    --external:vscode --platform=node --target=node20 --minify

echo "Copying to embed location..."
cp dist/bundle.js ../../src/antigravity_extension/extension.js
cp package.json ../../src/antigravity_extension/

echo "Done. Extension built and embedded at src/antigravity_extension/"
echo "Size: $(wc -c < ../../src/antigravity_extension/extension.js) bytes"
