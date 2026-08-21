FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1

WORKDIR /app

RUN pip install --no-cache-dir fastapi uvicorn pyjwt[crypto] cryptography

COPY server.py .

EXPOSE 9090

CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "9090"]
