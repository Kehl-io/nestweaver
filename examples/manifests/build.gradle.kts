plugins {
    kotlin("jvm") version "1.9.0"
    id("org.springframework.boot") version "3.1.0"
}

dependencies {
    implementation("org.springframework.boot:spring-boot-starter-web:3.1.0")
    implementation("com.fasterxml.jackson.core:jackson-databind:2.15.0")
    testImplementation("org.junit.jupiter:junit-jupiter:5.9.0")
}
