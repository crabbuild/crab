package org.crabbuild.qualification;

import java.net.URI;
import java.nio.charset.StandardCharsets;

import software.amazon.awssdk.auth.credentials.AwsCredentials;
import software.amazon.awssdk.auth.credentials.AwsBasicCredentials;
import software.amazon.awssdk.auth.credentials.AwsSessionCredentials;
import software.amazon.awssdk.auth.credentials.StaticCredentialsProvider;
import software.amazon.awssdk.core.ResponseBytes;
import software.amazon.awssdk.core.sync.RequestBody;
import software.amazon.awssdk.regions.Region;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.S3Configuration;
import software.amazon.awssdk.services.s3.model.GetObjectRequest;
import software.amazon.awssdk.services.s3.model.ListObjectsV2Request;
import software.amazon.awssdk.services.s3.model.PutObjectRequest;

public final class S3GatewayQualification {
  private S3GatewayQualification() {}

  private static String required(String name) {
    String value = System.getenv(name);
    if (value == null || value.isEmpty()) {
      throw new IllegalArgumentException(name + " is required");
    }
    return value;
  }

  public static void main(String[] args) {
    String endpoint = required("S3_GATEWAY_ECOSYSTEM_ENDPOINT");
    String bucket = required("S3_GATEWAY_ECOSYSTEM_BUCKET");
    String prefix = required("S3_GATEWAY_ECOSYSTEM_PREFIX");
    String region = required("S3_GATEWAY_ECOSYSTEM_REGION");
    String accessKey = required("S3_GATEWAY_ECOSYSTEM_ACCESS_KEY");
    String secretKey = required("S3_GATEWAY_ECOSYSTEM_SECRET_KEY");
    String sessionToken = System.getenv("S3_GATEWAY_ECOSYSTEM_SESSION_TOKEN");
    AwsCredentials credentials = sessionToken == null || sessionToken.isEmpty()
        ? AwsBasicCredentials.create(accessKey, secretKey)
        : AwsSessionCredentials.create(accessKey, secretKey, sessionToken);
    String key = prefix + "/java-sdk-v2/object.bin";
    byte[] body = "java-aws-sdk-v2-gateway-round-trip".getBytes(StandardCharsets.UTF_8);

    try (S3Client client = S3Client.builder()
        .endpointOverride(URI.create(endpoint))
        .region(Region.of(region))
        .credentialsProvider(StaticCredentialsProvider.create(credentials))
        .serviceConfiguration(S3Configuration.builder().pathStyleAccessEnabled(true).build())
        .build()) {
      client.putObject(
          PutObjectRequest.builder().bucket(bucket).key(key).build(),
          RequestBody.fromBytes(body));
      ResponseBytes<?> response = client.getObjectAsBytes(
          GetObjectRequest.builder().bucket(bucket).key(key).range("bytes=4-20").build());
      byte[] expected = java.util.Arrays.copyOfRange(body, 4, 21);
      if (!java.util.Arrays.equals(response.asByteArray(), expected)) {
        throw new IllegalStateException("range bytes differ");
      }
      var objects = client.listObjectsV2(
          ListObjectsV2Request.builder()
              .bucket(bucket)
              .prefix(prefix + "/java-sdk-v2/")
              .build())
          .contents();
      if (objects.size() != 1 || !objects.get(0).key().equals(key)) {
        throw new IllegalStateException("listing differs");
      }
      System.out.printf(
          "java-aws-sdk-v2:passed bytes=%d range_bytes=%d%n",
          body.length,
          response.asByteArray().length);
    }
  }
}
