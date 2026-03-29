FROM golang:1.24-alpine AS build

WORKDIR /app

RUN apk add --no-cache ca-certificates

# Сначала только модули — для кэширования зависимостей
COPY go.mod ./
RUN go mod download

# Затем весь исходный код
COPY . .

# Автоматически подтянуть все зависимости по импортам и сгенерировать go.sum
RUN go mod tidy

RUN go build -o astrix-server ./cmd/server

FROM alpine:3.19

WORKDIR /app

RUN apk add --no-cache ca-certificates

COPY --from=build /app/astrix-server /app/astrix-server

ENV HTTP_ADDR=0.0.0.0:8080

EXPOSE 8080

CMD ["/app/astrix-server"]

