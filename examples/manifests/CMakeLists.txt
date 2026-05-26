cmake_minimum_required(VERSION 3.20)
project(ExampleCApp)

find_package(Boost REQUIRED)
find_package(OpenSSL REQUIRED)

add_executable(example src/main.c)
target_link_libraries(example Boost::boost OpenSSL::SSL)
