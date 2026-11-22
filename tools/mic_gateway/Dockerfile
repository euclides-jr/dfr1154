FROM python:3.13-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY gateway.py index.html ./
USER 10001:10001
ENTRYPOINT ["python", "-u", "gateway.py"]
