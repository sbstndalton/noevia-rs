# Actual services app with only external runtime work replaced. SQLite startup is real.
import sys
sys.path.insert(0, sys.argv[1])
from app import model_files, hw
from app.main import app
import uvicorn
model_files.require_binary = lambda: None
hw.start_sampler = lambda: None
uvicorn.run(app, host="127.0.0.1", port=int(sys.argv[2]), log_level="critical")
