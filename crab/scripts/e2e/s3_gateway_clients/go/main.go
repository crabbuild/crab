package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"os"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

func required(name string) string {
	value := os.Getenv(name)
	if value == "" {
		panic(name + " is required")
	}
	return value
}

func main() {
	ctx := context.Background()
	endpoint := required("S3_GATEWAY_ECOSYSTEM_ENDPOINT")
	bucket := required("S3_GATEWAY_ECOSYSTEM_BUCKET")
	prefix := required("S3_GATEWAY_ECOSYSTEM_PREFIX")
	region := required("S3_GATEWAY_ECOSYSTEM_REGION")
	accessKey := required("S3_GATEWAY_ECOSYSTEM_ACCESS_KEY")
	secretKey := required("S3_GATEWAY_ECOSYSTEM_SECRET_KEY")
	clientConfig, err := config.LoadDefaultConfig(
		ctx,
		config.WithRegion(region),
		config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(
			accessKey, secretKey, os.Getenv("S3_GATEWAY_ECOSYSTEM_SESSION_TOKEN"),
		)),
	)
	if err != nil {
		panic(err)
	}
	client := s3.NewFromConfig(clientConfig, func(options *s3.Options) {
		options.BaseEndpoint = aws.String(endpoint)
		options.UsePathStyle = true
	})

	body := []byte("go-aws-sdk-v2-gateway-round-trip")
	key := prefix + "/go-sdk-v2/object.bin"
	_, err = client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: &bucket,
		Key:    &key,
		Body:   bytes.NewReader(body),
	})
	if err != nil {
		panic(err)
	}
	rangeHeader := "bytes=3-19"
	response, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: &bucket,
		Key:    &key,
		Range:  &rangeHeader,
	})
	if err != nil {
		panic(err)
	}
	defer response.Body.Close()
	actual, err := io.ReadAll(response.Body)
	if err != nil {
		panic(err)
	}
	expected := body[3:20]
	if sha256.Sum256(actual) != sha256.Sum256(expected) {
		panic("range bytes differ")
	}
	listing, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{
		Bucket: &bucket,
		Prefix: aws.String(prefix + "/go-sdk-v2/"),
	})
	if err != nil {
		panic(err)
	}
	if len(listing.Contents) != 1 || *listing.Contents[0].Key != key {
		panic("listing differs")
	}
	fmt.Printf("go-aws-sdk-v2:passed bytes=%d range_bytes=%d\n", len(body), len(actual))
}
