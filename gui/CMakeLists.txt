cmake_minimum_required(VERSION 3.16)

project(arexibogui LANGUAGES CXX)

set(CMAKE_AUTOUIC ON)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Qt6 REQUIRED COMPONENTS Widgets WebEngineWidgets WebChannel)

add_library(arexibogui STATIC
    view.cpp
    view.h
    lib.cpp
    lib.h
    res.qrc
)
target_link_libraries(arexibogui PRIVATE Qt6::WebEngineWidgets)
if(MSVC)
    target_compile_options(arexibogui PRIVATE /W3)
else()
    target_compile_options(arexibogui PRIVATE -Wall -pedantic)
endif()
install(TARGETS arexibogui)

# Create a dummy shared lib to discover libraries that Cargo needs to
# link against (not on Windows: build.rs names the Qt libraries itself)
if(NOT WIN32)
    add_library(dummy SHARED)
    target_link_libraries(dummy PUBLIC -lc
        Qt6::Core Qt6::Gui Qt6::Widgets Qt6::WebChannel
        Qt6::WebEngineCore Qt6::WebEngineWidgets)
endif()
