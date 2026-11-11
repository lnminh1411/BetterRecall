# Usage: Scripts/retrieval/run.sh <tokens>. WORK: scratch folder (img/, install/, outputs); PY: python with the main venv.
WORK=${WORK:-${BETTERRECALL_DEV:-$LOCALAPPDATA/BetterRecall-dev}/retrieval}
INS=${INS:-$WORK/install}
PY=${PY:-${BETTERRECALL_DEV:-$LOCALAPPDATA/BetterRecall-dev}/venv/Scripts/python.exe}
T=$1; H=$WORK/home$T; mkdir -p $H
$PY - $H $T <<'PY'
import json,os,sys
md=os.path.join(os.environ["LOCALAPPDATA"],"BetterRecall","models")
json.dump({"inference":{"fake":False,"modelDir":md,"imageTokens":int(sys.argv[2]),"idleExitS":600},"privacy":{"nsfwFilter":False}},open(os.path.join(sys.argv[1],"settings.json"),"w"))
PY
$INS/BetterRecall.Agent.exe $H $WORK/img $WORK/out$T.json
