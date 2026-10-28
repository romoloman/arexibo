cmake_minimum_required(VERSION 3.16)
project(codec-check LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 REQUIRED COMPONENTS Core Gui WebEngineCore)

add_executable(codec-check main.cpp)
target_link_libraries(codec-check PRIVATE Qt6::Core Qt6::Gui Qt6::WebEngineCore)
