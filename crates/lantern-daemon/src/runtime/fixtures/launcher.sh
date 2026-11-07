#!/usr/bin/env bash
lantern_fail() { echo "[lantern] $*" >&2; exit 78; }
cd '/work/feature x' || lantern_fail 'working directory missing'
. '/cache/shell-env.sh' || lantern_fail 'cannot source shell environment'
while IFS= read -r lantern_name; do unset "$lantern_name"; done < <(compgen -A variable LANTERN_)
unset lantern_name
case :"$PATH": in *:'/home/u/.cargo/bin':*) ;; *) export PATH="$PATH":'/home/u/.cargo/bin' ;; esac
export NVM_DIR='/home/u/.nvm'
. "$NVM_DIR/nvm.sh" --no-use && nvm use --silent || lantern_fail 'nvm: .nvmrc version not installed (run: nvm install)'
export VIRTUAL_ENV='/work/feature x/.venv'
export PATH="$VIRTUAL_ENV/bin:$PATH"
export NODE_ENV='development'
export LANTERN_BRANCH='x$(id)'
export LANTERN_DOMAIN='feature.app.glow'
export LANTERN_PORT='29100'
export LANTERN_PROJECT='app'
export LANTERN_INSTANCE='app_feature'
export LANTERN_INSTANCE_ID='app@feature'
export PORT="${LANTERN_PORT-}" DOMAIN="${LANTERN_DOMAIN-}" PROJECT_NAME="${LANTERN_PROJECT-}"
export API_KEY='a'\''b'
{ echo setup "${LANTERN_BRANCH}"
} || exit $?
exec bash -c 'pnpm dev --port ${LANTERN_PORT}' < <(exec sleep infinity)
