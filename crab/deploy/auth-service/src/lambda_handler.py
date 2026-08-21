"""AWS Lambda entry point using Mangum adapter.

Mangum translates API Gateway events into ASGI requests that FastAPI
can handle. This file is the Lambda handler — point your function
configuration at `src.lambda_handler.handler`.
"""

from mangum import Mangum

from src.app import app

# The Mangum adapter converts Lambda events to ASGI.
# Supports API Gateway REST API, HTTP API, and ALB.
handler = Mangum(app, lifespan="off")
